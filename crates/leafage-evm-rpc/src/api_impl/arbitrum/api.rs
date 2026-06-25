//! Arbitrum Orbit (Nitro) RPC impl.
//!
//! Transaction environment creation reuses mainnet's `create_mainnet_txn_env`
//! because Nitro's normal L2 execution is an Ethereum EVM transaction. Execution
//! uses an Arbitrum-specific EVM builder so ArbOS precompile addresses are
//! available in `eth_call` / gas simulation. The pre-execution hook stays at its
//! no-op default even on Prague: Arbitrum skips the EIP-2935 parent-blockhash
//! system call (go-ethereum-arb gates it on `!IsArbitrum`; block hashes come
//! from the per-block internal tx instead). Gas estimation still overrides
//! [`GasFeeHandler::estimate_l1_overhead`] to add Nitro's L1 data-posting cost
//! (posterGas), gated by the per-chain `enable_l1_gas` switch (off by default).

use crate::api_impl::arbitrum::evm::create_arbitrum_evm_from_state_with_env;
use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler, ToJsonRpcError, TxSetter};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use crate::error::{internal_rpc_err, rpc_error_with_code};
use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::sol_types::{decode_revert_reason, SolInterface, SolValue};
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arbitrum::arbos_state::ArbStateReader;
use leafage_evm_chains::arbitrum::precompile::{
    ArbitrumPrecompileEnv, NODE_INTERFACE_ADDRESS, NODE_INTERFACE_DEBUG_ADDRESS,
};
use leafage_evm_chains::arbitrum::tx::{
    ArbitrumSubmitRetryableTx, ArbitrumTxContext, ArbitrumTxEnv,
};
use leafage_evm_chains::arbitrum::{ArbitrumEvmConfig, ArbitrumHardfork};
use leafage_evm_storage::BlockIndex;
use leafage_evm_types::{
    BlockEnv, BlockId, BlockInfo, BlockNumberOrTag, CallRequest, CfgEnv, DebankErrorCode,
};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::Transaction as _;
use revm::inspector::NoOpInspector;
use revm::primitives::TxKind;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::collections::HashMap;
use std::fmt::Debug;

type ArbitrumApiImpl<DB> = ApiImpl<DB, ArbitrumHardfork, ArbitrumEvmConfig>;

const ARBITRUM_ONE_NITRO_GENESIS_BLOCK: u64 = 22_207_818;
const ARBOS_VERSION_40: u64 = 40;

alloy::sol! {
    interface INodeInterfaceVirtual {
        function estimateRetryableTicket(
            address sender,
            uint256 deposit,
            address to,
            uint256 l2CallValue,
            address excessFeeRefundAddress,
            address callValueRefundAddress,
            bytes calldata data
        ) external;
        function constructOutboxProof(uint64 size, uint64 leaf)
            external
            view
            returns (bytes32 send, bytes32 root, bytes32[] memory proof);
        function findBatchContainingBlock(uint64 blockNum) external view returns (uint64 batch);
        function getL1Confirmations(bytes32 blockHash) external view returns (uint64 confirmations);
        function gasEstimateComponents(address to, bool contractCreation, bytes calldata data)
            external
            payable
            returns (uint64 gasEstimate, uint64 gasEstimateForL1, uint256 baseFee, uint256 l1BaseFeeEstimate);
        function gasEstimateL1Component(address to, bool contractCreation, bytes calldata data)
            external
            payable
            returns (uint64 gasEstimateForL1, uint256 baseFee, uint256 l1BaseFeeEstimate);
        function legacyLookupMessageBatchProof(uint256 batchNum, uint64 index)
            external
            view
            returns (
                bytes32[] memory proof,
                uint256 path,
                address l2Sender,
                address l1Dest,
                uint256 l2Block,
                uint256 l1Block,
                uint256 timestamp,
                uint256 amount,
                bytes memory calldataForL1
            );
        function nitroGenesisBlock() external pure returns (uint256 number);
        function blockL1Num(uint64 l2BlockNum) external view returns (uint64 l1BlockNum);
        function l2BlockRangeForL1(uint64 blockNum) external view returns (uint64 firstBlock, uint64 lastBlock);
    }

    interface INodeInterfaceDebugVirtual {
        struct RetryableInfo {
            uint64 timeout;
            address from;
            address to;
            uint256 value;
            address beneficiary;
            uint64 tries;
            bytes data;
        }

        function getRetryable(bytes32 ticket)
            external
            view
            returns (RetryableInfo memory retryable);
    }
}

fn precompile_env<StateDB: DatabaseRef>(
    block_env: &BlockEnv,
    state: &StateDB,
    tx: &ArbitrumTxEnv,
    custom_cfg: Option<&ArbitrumEvmConfig>,
) -> ArbitrumPrecompileEnv {
    ArbitrumPrecompileEnv {
        current_arbos_version: state.arbos_version(),
        current_tx_l1_gas_fees: state.current_tx_l1_gas_fee(&tx.base, block_env.basefee),
        current_l1_block_number: tx.context.current_l1_block_number,
        current_retryable_ticket: tx.retryable.as_ref().and_then(|ctx| ctx.ticket_id),
        current_refund_to: tx.retryable.as_ref().map(|ctx| ctx.refund_to),
        allow_debug_precompiles: custom_cfg.is_some_and(|cfg| cfg.allow_debug_precompiles),
        current_chain_config: custom_cfg
            .and_then(|cfg| cfg.chain_config.as_ref())
            .map(|chain_config| Bytes::copy_from_slice(chain_config.get().as_bytes())),
    }
}

fn header_l1_block_num(block: &BlockInfo, legacy_zero_base_fee_until: u64) -> u64 {
    if block.header.base_fee_per_gas.is_none()
        || block.header.extra_data.len() != 32
        || block.header.difficulty != U256::from(1)
    {
        return 0;
    }

    let mix = block.header.mix_hash.as_slice();
    let arbos_format_version = u64::from_be_bytes(
        mix[16..24]
            .try_into()
            .expect("fixed-size header mix digest slice"),
    );
    if arbos_format_version <= ARBOS_VERSION_40
        && block.header.base_fee_per_gas == Some(0)
        && block.header.timestamp < legacy_zero_base_fee_until
    {
        return 0;
    }

    u64::from_be_bytes(
        mix[8..16]
            .try_into()
            .expect("fixed-size header mix digest slice"),
    )
}

fn block_by_l2_num<DB>(db: &DB, l2_block_num: u64) -> RpcResult<BlockInfo>
where
    DB: BlockIndex,
{
    if l2_block_num > i64::MAX as u64 {
        return Err(internal_rpc_err(format!(
            "requested l2 block number {l2_block_num} out of range for int64"
        )));
    }

    db.get_block_by_id(BlockId::Number(BlockNumberOrTag::Number(l2_block_num)))
        .map_err(|err| internal_rpc_err(err.to_string()))?
        .ok_or_else(|| internal_rpc_err(format!("nil header for l2 block: {l2_block_num}")))
}

fn block_l1_num<DB>(db: &DB, l2_block_num: u64, legacy_zero_base_fee_until: u64) -> RpcResult<u64>
where
    DB: BlockIndex,
{
    let block = block_by_l2_num(db, l2_block_num)?;
    Ok(header_l1_block_num(&block, legacy_zero_base_fee_until))
}

fn latest_l2_block_num<DB>(db: &DB) -> RpcResult<u64>
where
    DB: BlockIndex,
{
    let block = db
        .get_block_by_id(BlockId::Number(BlockNumberOrTag::Latest))
        .map_err(|err| internal_rpc_err(err.to_string()))?
        .ok_or_else(|| internal_rpc_err("nil latest l2 block header"))?;
    Ok(block.header.number)
}

fn configured_nitro_genesis_block_num(chain_id: u64, config: Option<&ArbitrumEvmConfig>) -> u64 {
    config
        .and_then(|config| config.genesis_block_num)
        .unwrap_or_else(|| match chain_id {
            // Nitro chain info: Arbitrum One's first Nitro block after classic.
            42161 => ARBITRUM_ONE_NITRO_GENESIS_BLOCK,
            _ => 0,
        })
}

fn configured_legacy_zero_base_fee_until(config: Option<&ArbitrumEvmConfig>) -> u64 {
    config
        .map(|config| config.legacy_zero_base_fee_until)
        .unwrap_or_default()
}

fn first_l2_block_for_l1<DB>(
    db: &DB,
    genesis_block_num: u64,
    current_block_num: u64,
    target_l1_block_num: u64,
    legacy_zero_base_fee_until: u64,
    cached_l1_nums: &mut HashMap<u64, u64>,
) -> RpcResult<u64>
where
    DB: BlockIndex,
{
    let mut low = genesis_block_num;
    let mut high = current_block_num;

    if block_l1_num(db, high, legacy_zero_base_fee_until)? < target_l1_block_num {
        return Ok(high.saturating_add(1));
    }

    while low < high {
        let mid = low + (high - low) / 2;
        let mid_l1_block_num = match cached_l1_nums.get(&mid) {
            Some(l1_block_num) => *l1_block_num,
            None => {
                let l1_block_num = block_l1_num(db, mid, legacy_zero_base_fee_until)?;
                cached_l1_nums.insert(mid, l1_block_num);
                l1_block_num
            }
        };

        if mid_l1_block_num < target_l1_block_num {
            low = mid + 1;
        } else {
            high = mid;
        }
    }

    Ok(high)
}

fn match_l2_block_num_with_l1<DB>(
    db: &DB,
    l2_block_num: u64,
    l1_block_num: u64,
    legacy_zero_base_fee_until: u64,
) -> RpcResult<()>
where
    DB: BlockIndex,
{
    let block_l1_block_num =
        block_l1_num(db, l2_block_num, legacy_zero_base_fee_until).map_err(|err| {
            internal_rpc_err(format!(
                "failed to get the L1 block number of the L2 block: {l2_block_num}. Error: {}",
                err.message()
            ))
        })?;
    if block_l1_block_num != l1_block_num {
        return Err(internal_rpc_err(format!(
            "no L2 block was found with the given L1 block number. Found L2 block: {l2_block_num} with L1 block number: {block_l1_block_num}, given L1 block number: {l1_block_num}",
        )));
    }
    Ok(())
}

fn l2_block_range_for_l1<DB>(
    db: &DB,
    genesis_block_num: u64,
    current_block_num: u64,
    l1_block_num: u64,
    legacy_zero_base_fee_until: u64,
) -> RpcResult<(u64, u64)>
where
    DB: BlockIndex,
{
    let mut cached_l1_nums = HashMap::new();
    let first_block = first_l2_block_for_l1(
        db,
        genesis_block_num,
        current_block_num,
        l1_block_num,
        legacy_zero_base_fee_until,
        &mut cached_l1_nums,
    )
    .map_err(|err| {
        internal_rpc_err(format!(
            "failed to get the first L2 block with the L1 block: {l1_block_num}. Error: {}",
            err.message()
        ))
    })?;
    let next_l1_block_num = l1_block_num.wrapping_add(1);
    let last_block_exclusive = first_l2_block_for_l1(
        db,
        genesis_block_num,
        current_block_num,
        next_l1_block_num,
        legacy_zero_base_fee_until,
        &mut cached_l1_nums,
    )
    .map_err(|err| {
        internal_rpc_err(format!(
            "failed to get the last L2 block with the L1 block: {l1_block_num}. Error: {}",
            err.message()
        ))
    })?;

    match_l2_block_num_with_l1(db, first_block, l1_block_num, legacy_zero_base_fee_until)?;
    let last_block = last_block_exclusive.wrapping_sub(1);
    match_l2_block_num_with_l1(db, last_block, l1_block_num, legacy_zero_base_fee_until)?;

    Ok((first_block, last_block))
}

impl<DB> ArbitrumApiImpl<DB> {
    fn remap_l1_address(address: Address) -> Address {
        const ADDRESS_ALIAS_OFFSET: U256 =
            alloy::primitives::uint!(0x1111000000000000000000000000000000001111_U256);

        let value = U256::from_be_slice(address.as_slice());
        let mask = (U256::from(1u8) << 160) - U256::from(1u8);
        let aliased: U256 = value.wrapping_add(ADDRESS_ALIAS_OFFSET) & mask;
        let bytes = aliased.to_be_bytes::<32>();
        Address::from_slice(&bytes[12..])
    }

    fn retryable_redeem_request(
        request: &CallRequest,
        call: &INodeInterfaceVirtual::estimateRetryableTicketCall,
        retry_gas_price: u128,
    ) -> CallRequest {
        let mut target_request = request.clone();
        target_request.from = Some(Self::remap_l1_address(call.sender));
        target_request.to = if call.to == Address::ZERO {
            TxKind::Create.into()
        } else {
            TxKind::Call(call.to).into()
        };
        target_request.value = Some(call.l2CallValue);
        target_request.input = call.data.clone().into();
        target_request.nonce = Some(0);
        target_request.gas_price = Some(retry_gas_price);
        target_request.max_fee_per_gas = None;
        target_request.max_priority_fee_per_gas = None;
        target_request
    }

    fn retryable_redeem_call_from_node_interface(
        request: &CallRequest,
    ) -> Option<INodeInterfaceVirtual::estimateRetryableTicketCall> {
        let Some(TxKind::Call(to)) = request.to else {
            return None;
        };
        if to != NODE_INTERFACE_ADDRESS {
            return None;
        }

        let data = request
            .input
            .input()
            .map(|data| data.as_ref())
            .unwrap_or(&[]);

        let Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::estimateRetryableTicket(call)) =
            INodeInterfaceVirtual::INodeInterfaceVirtualCalls::abi_decode(data)
        else {
            return None;
        };

        Some(call)
    }

    fn retryable_redeem_gas_price(source_message_gas_price: u128, block_env: &BlockEnv) -> u128 {
        if source_message_gas_price == 0 {
            0
        } else {
            block_env.basefee as u128
        }
    }

    fn cfg_for_tx(&self, tx: &ArbitrumTxEnv) -> CfgEnv<ArbitrumHardfork> {
        let mut cfg = self.evm_cfg.cfg.clone();
        if tx.is_retryable_redeem() {
            cfg.disable_balance_check = true;
            cfg.disable_nonce_check = true;
            cfg.disable_eip3607 = true;
            if tx.is_zero_gas_price_retryable() {
                cfg.disable_base_fee = true;
            }
        }
        cfg
    }

    fn block_env_for_tx(&self, block_env: &BlockEnv, tx: &ArbitrumTxEnv) -> BlockEnv {
        let mut block_env = block_env.clone();
        if tx.is_zero_gas_price_retryable() {
            block_env.basefee = 0;
        }
        block_env
    }

    fn tx_context_for_block(&self, block: &BlockInfo) -> ArbitrumTxContext {
        ArbitrumTxContext {
            current_l1_block_number: header_l1_block_num(
                block,
                configured_legacy_zero_base_fee_until(self.evm_cfg.custom_cfg.as_ref()),
            ),
        }
    }
}

// create_txn_env reuses mainnet's free function; execution uses Arbitrum
// precompiles. apply_pre_execution_changes keeps the trait default (no-op) —
// see module doc.
impl<DB> EvmExecutor for ArbitrumApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = ArbitrumTxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        let context = self.tx_context_for_block(block);
        let retryable_call = (db.arbos_version() != 0)
            .then(|| Self::retryable_redeem_call_from_node_interface(&request))
            .flatten();
        let (request, retryable_context) = match retryable_call {
            Some(call) => {
                let source_tx = create_mainnet_txn_env(
                    block_env,
                    self.evm_cfg.cfg.clone(),
                    request.clone(),
                    &db,
                    chain_id,
                )?;
                let source_message_gas_price =
                    source_tx.effective_gas_price(block_env.basefee as u128);
                let retry_gas_price =
                    Self::retryable_redeem_gas_price(source_message_gas_price, block_env);
                let l1_base_fee = db
                    .read_pricing()
                    .map(|pricing| pricing.price_per_unit)
                    .unwrap_or_default();
                let max_submission_fee =
                    ArbitrumSubmitRetryableTx::submission_fee(call.data.len(), l1_base_fee);
                let retry_to = (call.to != Address::ZERO).then_some(call.to);
                let submit_tx = ArbitrumSubmitRetryableTx {
                    chain_id: U256::ZERO,
                    request_id: B256::ZERO,
                    from: Self::remap_l1_address(call.sender),
                    l1_base_fee,
                    deposit_value: call.deposit,
                    gas_fee_cap: U256::from(source_message_gas_price),
                    gas: source_tx.gas_limit,
                    retry_to,
                    retry_value: call.l2CallValue,
                    beneficiary: call.callValueRefundAddress,
                    max_submission_fee,
                    fee_refund_addr: call.excessFeeRefundAddress,
                    retry_data: call.data.clone(),
                };
                let ticket_id = submit_tx.ticket_id();
                let refund_to = call.excessFeeRefundAddress;
                (
                    Self::retryable_redeem_request(&request, &call, retry_gas_price),
                    Some((ticket_id, refund_to)),
                )
            }
            None => (request, None),
        };
        let base =
            create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)?;
        Ok(match retryable_context {
            Some((ticket_id, refund_to)) => {
                ArbitrumTxEnv::retryable_redeem(base, Some(ticket_id), refund_to)
            }
            None => ArbitrumTxEnv::new(base),
        }
        .with_context(context))
    }

    fn transact<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let block_env = self.block_env_for_tx(block_env, &tx);
        let precompile_env =
            precompile_env(&block_env, &state, &tx, self.evm_cfg.custom_cfg.as_ref());
        let mut evm = create_arbitrum_evm_from_state_with_env(
            block_env,
            self.cfg_for_tx(&tx),
            state,
            NoOpInspector {},
            precompile_env,
        );

        evm.transact(tx).map(|res| res.result.into())
    }

    fn inspect_tx_commit<
        StateDB: DatabaseRef + DatabaseCommit,
        R,
        F: FnOnce(TracingInspector) -> R,
    >(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let mut inspector = TracingInspector::new(inspector_cfg);
        let block_env = self.block_env_for_tx(block_env, &tx);
        let precompile_env =
            precompile_env(&block_env, &state, &tx, self.evm_cfg.custom_cfg.as_ref());
        let mut evm = create_arbitrum_evm_from_state_with_env(
            block_env,
            self.cfg_for_tx(&tx),
            state,
            &mut inspector,
            precompile_env,
        );

        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}

impl TxSetter for ArbitrumTxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.gas_limit = gas_limit;
    }
}

impl<DB> GasFeeHandler for ArbitrumApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = ArbitrumTxEnv;

    fn gas_allowance<StateDB: DatabaseRef>(
        &self,
        _request: &CallRequest,
        tx: &Self::Tx,
        state: &StateDB,
        _block_env: &BlockEnv,
    ) -> RpcResult<u64> {
        if tx.is_retryable_redeem() {
            return Ok(u64::MAX);
        }

        let caller = state.basic_ref(tx.caller()).map_err(|err| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, err.to_string())
        })?;
        let balance = caller
            .map(|account| account.balance)
            .unwrap_or_default()
            .checked_sub(tx.value())
            .ok_or_else(|| {
                rpc_error_with_code(
                    DebankErrorCode::BalanceExhausted as i32,
                    "Insufficient funds".to_string(),
                )
            })?;
        Ok(balance
            .checked_div(U256::from(tx.gas_price()))
            .unwrap_or_default()
            .try_into()
            .unwrap())
    }

    fn estimate_l1_overhead<StateDB>(
        &self,
        _block: &BlockInfo,
        block_env: &BlockEnv,
        tx: Self::Tx,
        state: &StateDB,
    ) -> u64
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        // Per-chain opt-in: off (other arb chains / no config) → behave like mainnet.
        if !self
            .evm_cfg
            .custom_cfg
            .as_ref()
            .is_some_and(|c| c.enable_l1_gas)
        {
            return 0;
        }

        // Pricing read straight from ArbOS state; missing / pre-pricing → 0 (safe degrade).
        let pricing = match state.read_pricing() {
            Some(p) => p,
            None => return 0,
        };

        pricing.poster_gas(&tx.base, block_env.basefee)
    }
}

impl<DB> ApiCore for ArbitrumApiImpl<DB>
where
    DB: BlockIndex + Sync + Send + 'static,
{
    fn handle_virtual_call<StateDB, EstimateGas>(
        &self,
        request: &CallRequest,
        block: &BlockInfo,
        block_env: &BlockEnv,
        state: &StateDB,
        mut estimate_gas: EstimateGas,
    ) -> RpcResult<Option<Bytes>>
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
        EstimateGas: FnMut(CallRequest) -> RpcResult<(u64, u64)>,
    {
        let Some(TxKind::Call(to)) = request.to else {
            return Ok(None);
        };
        if state.arbos_version() == 0 {
            return Ok(None);
        }
        if to == NODE_INTERFACE_DEBUG_ADDRESS {
            let data = request
                .input
                .input()
                .map(|data| data.as_ref())
                .unwrap_or(&[]);
            return match INodeInterfaceDebugVirtual::INodeInterfaceDebugVirtualCalls::abi_decode(
                data,
            ) {
                Ok(INodeInterfaceDebugVirtual::INodeInterfaceDebugVirtualCalls::getRetryable(
                    call,
                )) => {
                    let info = state
                        .read_retryable_info(call.ticket)
                        .map_err(internal_rpc_err)?
                        .ok_or_else(|| {
                            internal_rpc_err(format!(
                                "no retryable with id {:?} exists",
                                call.ticket
                            ))
                        })?;
                    let ret = INodeInterfaceDebugVirtual::RetryableInfo {
                        timeout: info.timeout,
                        from: info.from,
                        to: info.to.unwrap_or_default(),
                        value: info.value,
                        beneficiary: info.beneficiary,
                        tries: info.tries,
                        data: info.data,
                    };
                    Ok(Some(ret.abi_encode().into()))
                }
                Err(err) => Err(internal_rpc_err(format!(
                    "invalid NodeInterfaceDebug calldata: {err}",
                ))),
            };
        }

        if to != NODE_INTERFACE_ADDRESS {
            return Ok(None);
        }

        let data = request
            .input
            .input()
            .map(|data| data.as_ref())
            .unwrap_or(&[]);

        match INodeInterfaceVirtual::INodeInterfaceVirtualCalls::abi_decode(data) {
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::gasEstimateComponents(call)) => {
                if call.to == NODE_INTERFACE_ADDRESS || call.to == NODE_INTERFACE_DEBUG_ADDRESS {
                    return Err(internal_rpc_err("cannot estimate virtual contract"));
                }

                let mut target_request = request.clone();
                target_request.to = if call.contractCreation {
                    TxKind::Create.into()
                } else {
                    TxKind::Call(call.to).into()
                };
                target_request.input = call.data.clone().into();
                target_request.nonce = None;

                let target_tx = self.create_txn_env(
                    block,
                    block_env,
                    target_request.clone(),
                    state,
                    self.evm_cfg.cfg.chain_id,
                )?;
                let pricing = state.read_pricing();
                let l1_gas = pricing
                    .as_ref()
                    .map(|pricing| pricing.poster_gas(&target_tx.base, block_env.basefee))
                    .unwrap_or_default();
                let l1_base_fee = pricing
                    .as_ref()
                    .map(|pricing| pricing.price_per_unit)
                    .unwrap_or_default();
                let (execution_gas, _) = estimate_gas(target_request)?;
                let gas_estimate = execution_gas.saturating_add(l1_gas);
                let ret = (
                    gas_estimate,
                    l1_gas,
                    U256::from(block_env.basefee),
                    l1_base_fee,
                )
                    .abi_encode();
                Ok(Some(ret.into()))
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::gasEstimateL1Component(call)) => {
                let mut target_request = request.clone();
                target_request.to = if call.contractCreation {
                    TxKind::Create.into()
                } else {
                    TxKind::Call(call.to).into()
                };
                target_request.input = call.data.clone().into();
                target_request.nonce = None;

                let target_tx = self.create_txn_env(
                    block,
                    block_env,
                    target_request,
                    state,
                    self.evm_cfg.cfg.chain_id,
                )?;
                let pricing = state.read_pricing();
                let l1_gas = pricing
                    .as_ref()
                    .map(|pricing| pricing.poster_gas(&target_tx.base, block_env.basefee))
                    .unwrap_or_default();
                let l1_base_fee = pricing
                    .as_ref()
                    .map(|pricing| pricing.price_per_unit)
                    .unwrap_or_default();
                let ret = (l1_gas, U256::from(block_env.basefee), l1_base_fee).abi_encode();
                Ok(Some(ret.into()))
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::nitroGenesisBlock(_)) => {
                Ok(Some(
                    configured_nitro_genesis_block_num(
                        self.evm_cfg.cfg.chain_id,
                        self.evm_cfg.custom_cfg.as_ref(),
                    )
                    .abi_encode()
                    .into(),
                ))
            }
            Ok(
                INodeInterfaceVirtual::INodeInterfaceVirtualCalls::legacyLookupMessageBatchProof(_),
            ) => Err(internal_rpc_err(
                "this node doesnt support classicLookupMessageBatchProof",
            )),
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::estimateRetryableTicket(_)) => {
                let tx = self.create_txn_env(
                    block,
                    block_env,
                    request.clone(),
                    state,
                    self.evm_cfg.cfg.chain_id,
                )?;
                match self
                    .transact(block_env, state, tx)
                    .map_err(|err| err.to_rpc_error())?
                {
                    ExecutionResult::Success { output, .. } => {
                        Ok(Some(output.into_data().0.into()))
                    }
                    ExecutionResult::Revert { output, .. } => Err(internal_rpc_err(format!(
                        "Reverted: {:?}",
                        decode_revert_reason(&output).unwrap_or("execution revert".to_string())
                    ))),
                    ExecutionResult::Halt { reason, .. } => {
                        Err(internal_rpc_err(format!("Halted: {:?}", reason)))
                    }
                }
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::blockL1Num(call)) => Ok(Some(
                block_l1_num(
                    &self.db,
                    call.l2BlockNum,
                    configured_legacy_zero_base_fee_until(self.evm_cfg.custom_cfg.as_ref()),
                )?
                .abi_encode()
                .into(),
            )),
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::l2BlockRangeForL1(call)) => {
                let genesis_block_num = configured_nitro_genesis_block_num(
                    self.evm_cfg.cfg.chain_id,
                    self.evm_cfg.custom_cfg.as_ref(),
                );
                let legacy_zero_base_fee_until =
                    configured_legacy_zero_base_fee_until(self.evm_cfg.custom_cfg.as_ref());
                let current_block_num = latest_l2_block_num(&self.db)?;
                let (first_block, last_block) = l2_block_range_for_l1(
                    &self.db,
                    genesis_block_num,
                    current_block_num,
                    call.blockNum,
                    legacy_zero_base_fee_until,
                )?;
                Ok(Some((first_block, last_block).abi_encode().into()))
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::getL1Confirmations(_)) => Err(
                internal_rpc_err("NodeInterface.getL1Confirmations requires node backend context"),
            ),
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::constructOutboxProof(_)) => {
                Err(internal_rpc_err(
                    "NodeInterface.constructOutboxProof requires node backend context",
                ))
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::findBatchContainingBlock(_)) => {
                Err(internal_rpc_err(
                    "NodeInterface.findBatchContainingBlock requires node backend context",
                ))
            }
            Err(err) => Err(internal_rpc_err(format!(
                "invalid NodeInterface calldata: {err}",
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_redeem_request_matches_nitro_message_swap_fields() {
        let mut request = CallRequest::default();
        request.to = Some(TxKind::Call(NODE_INTERFACE_ADDRESS));
        request.gas = Some(777_000);
        request.gas_price = Some(999);

        let sender = Address::ZERO;
        let target = Address::with_last_byte(0xaa);
        let fee_refund = Address::with_last_byte(0xbb);
        let call_value_refund = Address::with_last_byte(0xcc);
        let data = Bytes::from_static(&[1, 2, 3, 4]);
        let call = INodeInterfaceVirtual::estimateRetryableTicketCall {
            sender,
            deposit: U256::from(1_000_000u64),
            to: target,
            l2CallValue: U256::from(7u64),
            excessFeeRefundAddress: fee_refund,
            callValueRefundAddress: call_value_refund,
            data: data.clone(),
        };

        let target_request = ArbitrumApiImpl::<()>::retryable_redeem_request(&request, &call, 999);

        assert_eq!(
            target_request.from,
            Some(alloy::primitives::address!(
                "1111000000000000000000000000000000001111"
            ))
        );
        assert_eq!(target_request.to, Some(TxKind::Call(target)));
        assert_eq!(target_request.value, Some(U256::from(7u64)));
        assert_eq!(
            target_request.input.input().map(|input| input.as_ref()),
            Some(data.as_ref())
        );
        assert_eq!(target_request.gas, Some(777_000));
        assert_eq!(target_request.gas_price, Some(999));
        assert!(target_request.max_fee_per_gas.is_none());
        assert!(target_request.max_priority_fee_per_gas.is_none());
        assert_eq!(call.excessFeeRefundAddress, fee_refund);
    }

    #[test]
    fn retryable_redeem_call_from_node_interface_decodes_virtual_call() {
        let fee_refund = Address::with_last_byte(0xbb);
        let target = Address::with_last_byte(0xaa);
        let call = INodeInterfaceVirtual::estimateRetryableTicketCall {
            sender: Address::ZERO,
            deposit: U256::from(1_000_000u64),
            to: target,
            l2CallValue: U256::from(7u64),
            excessFeeRefundAddress: fee_refund,
            callValueRefundAddress: Address::with_last_byte(0xcc),
            data: Bytes::from_static(&[1, 2, 3, 4]),
        };

        let mut request = CallRequest::default();
        request.to = Some(TxKind::Call(NODE_INTERFACE_ADDRESS));
        request.input = Bytes::from(
            INodeInterfaceVirtual::INodeInterfaceVirtualCalls::estimateRetryableTicket(
                call.clone(),
            )
            .abi_encode(),
        )
        .into();

        let decoded_call =
            ArbitrumApiImpl::<()>::retryable_redeem_call_from_node_interface(&request)
                .expect("estimateRetryableTicket should decode");
        let refund_to = decoded_call.excessFeeRefundAddress;
        let target_request =
            ArbitrumApiImpl::<()>::retryable_redeem_request(&request, &decoded_call, 42);

        assert_eq!(refund_to, fee_refund);
        assert_eq!(target_request.to, Some(TxKind::Call(target)));
        assert_eq!(target_request.value, Some(U256::from(7u64)));
        assert_eq!(
            target_request.input.input().map(|input| input.as_ref()),
            Some(call.data.as_ref())
        );
    }

    #[test]
    fn retryable_redeem_gas_price_matches_scheduled_retry_basefee() {
        let mut block_env = BlockEnv::default();
        block_env.basefee = 42;

        assert_eq!(
            ArbitrumApiImpl::<()>::retryable_redeem_gas_price(0, &block_env),
            0
        );
        assert_eq!(
            ArbitrumApiImpl::<()>::retryable_redeem_gas_price(7, &block_env),
            42
        );
    }

    #[test]
    fn retryable_redeem_request_uses_create_for_zero_target() {
        let call = INodeInterfaceVirtual::estimateRetryableTicketCall {
            sender: Address::ZERO,
            deposit: U256::ZERO,
            to: Address::ZERO,
            l2CallValue: U256::ZERO,
            excessFeeRefundAddress: Address::ZERO,
            callValueRefundAddress: Address::ZERO,
            data: Bytes::new(),
        };

        let target_request =
            ArbitrumApiImpl::<()>::retryable_redeem_request(&CallRequest::default(), &call, 0);

        assert_eq!(target_request.to, Some(TxKind::Create));
        assert_eq!(target_request.gas_price, Some(0));
    }
}
