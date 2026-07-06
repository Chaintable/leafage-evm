use crate::api_impl::arbitrum::api::ArbitrumApiImpl;
use crate::error::internal_rpc_err;
use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::sol_types::{decode_revert_reason, SolInterface, SolValue};
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arbitrum::arbos_state::ArbStateReader;
use leafage_evm_chains::arbitrum::precompile::{
    NODE_INTERFACE_ADDRESS, NODE_INTERFACE_DEBUG_ADDRESS,
};
use leafage_evm_chains::arbitrum::tx::{ArbitrumSubmitRetryableTx, ArbitrumTxEnv};
use leafage_evm_chains::arbitrum::ArbitrumEvmConfig;
use leafage_evm_storage::BlockIndex;
use leafage_evm_types::{BlockEnv, BlockId, BlockInfo, BlockNumberOrTag};
use revm::context::result::{
    EVMError, ExecutionResult, HaltReason, InvalidTransaction, Output, ResultGas, SuccessReason,
};
use revm::context::Transaction as _;
use revm::primitives::{StorageKey, StorageValue, TxKind};
use revm::state::{AccountInfo, Bytecode};
use revm::DatabaseRef;
use std::collections::HashMap;
use std::fmt::Debug;

const ARBITRUM_ONE_NITRO_GENESIS_BLOCK: u64 = 22_207_817;
const ARBOS_VERSION_40: u64 = 40;
const MIN_TRANSACTION_GAS: u64 = 21_000;
const CALL_STIPEND_GAS: u64 = 2_300;
const ESTIMATE_GAS_ERROR_RATIO: f64 = 0.015;

fn random_gas_for_l1_component() -> u64 {
    u32::from_be_bytes(
        keccak256("Gas").as_slice()[..4]
            .try_into()
            .expect("fixed-size keccak prefix"),
    ) as u64
}

#[derive(Clone, Copy, Debug)]
struct BorrowedState<'a, DB: ?Sized>(&'a DB);

impl<DB> DatabaseRef for BorrowedState<'_, DB>
where
    DB: DatabaseRef + ?Sized,
{
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.0.code_by_hash_ref(code_hash)
    }

    fn storage_ref(
        &self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        self.0.storage_ref(address, index)
    }

    fn storage_by_account_id_ref(
        &self,
        address: Address,
        account_id: usize,
        storage_key: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        self.0
            .storage_by_account_id_ref(address, account_id, storage_key)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.0.block_hash_ref(number)
    }
}

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

#[derive(Clone, Debug)]
pub(in crate::api_impl::arbitrum) struct RetryableRedeemCall {
    pub sender: Address,
    pub deposit: U256,
    pub to: Address,
    pub l2_call_value: U256,
    pub excess_fee_refund_address: Address,
    pub call_value_refund_address: Address,
    pub data: Bytes,
}

impl From<INodeInterfaceVirtual::estimateRetryableTicketCall> for RetryableRedeemCall {
    fn from(call: INodeInterfaceVirtual::estimateRetryableTicketCall) -> Self {
        Self {
            sender: call.sender,
            deposit: call.deposit,
            to: call.to,
            l2_call_value: call.l2CallValue,
            excess_fee_refund_address: call.excessFeeRefundAddress,
            call_value_refund_address: call.callValueRefundAddress,
            data: call.data,
        }
    }
}

pub(in crate::api_impl::arbitrum) fn header_l1_block_num(
    block: &BlockInfo,
    legacy_zero_base_fee_until: u64,
) -> u64 {
    if block.header.base_fee_per_gas.is_none()
        || block.header.extra_data.len() != 32
        || block.header.difficulty != U256::from(1)
    {
        return 0;
    }

    let mix = block.header.mix_hash.as_slice();
    let read_u64 = |range: std::ops::Range<usize>| {
        mix.get(range)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_be_bytes)
    };
    let Some(arbos_format_version) = read_u64(16..24) else {
        return 0;
    };
    if arbos_format_version <= ARBOS_VERSION_40
        && block.header.base_fee_per_gas == Some(0)
        && block.header.timestamp < legacy_zero_base_fee_until
    {
        return 0;
    }

    read_u64(8..16).unwrap_or_default()
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

pub(in crate::api_impl::arbitrum) fn configured_legacy_zero_base_fee_until(
    config: Option<&ArbitrumEvmConfig>,
) -> u64 {
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
    pub(in crate::api_impl::arbitrum) fn remap_l1_address(address: Address) -> Address {
        const ADDRESS_ALIAS_OFFSET: U256 =
            alloy::primitives::uint!(0x1111000000000000000000000000000000001111_U256);

        let value = U256::from_be_slice(address.as_slice());
        let mask = (U256::from(1u8) << 160) - U256::from(1u8);
        let aliased: U256 = value.wrapping_add(ADDRESS_ALIAS_OFFSET) & mask;
        let bytes = aliased.to_be_bytes::<32>();
        Address::from_slice(&bytes[12..])
    }

    pub(in crate::api_impl::arbitrum) fn retryable_redeem_gas_price(
        source_message_gas_price: u128,
        block_env: &BlockEnv,
    ) -> u128 {
        if source_message_gas_price == 0 {
            0
        } else {
            block_env.basefee as u128
        }
    }

    fn virtual_success(tx: &ArbitrumTxEnv, output: Bytes) -> ExecutionResult<HaltReason> {
        ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: ResultGas::new(tx.gas_limit(), 0, 0, 0, 0),
            logs: Vec::new(),
            output: Output::Call(output),
        }
    }

    fn retryable_redeem_tx<StateDB>(
        source_tx: &ArbitrumTxEnv,
        block_env: &BlockEnv,
        state: &StateDB,
        call: RetryableRedeemCall,
    ) -> ArbitrumTxEnv
    where
        StateDB: DatabaseRef,
    {
        let source_message_gas_price = source_tx.effective_gas_price(block_env.basefee as u128);
        let retry_gas_price = Self::retryable_redeem_gas_price(source_message_gas_price, block_env);
        let l1_base_fee = state
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
            gas: source_tx.gas_limit(),
            retry_to,
            retry_value: call.l2_call_value,
            beneficiary: call.call_value_refund_address,
            max_submission_fee,
            fee_refund_addr: call.excess_fee_refund_address,
            retry_data: call.data.clone(),
        };

        let mut base = source_tx.base.clone();
        base.caller = Self::remap_l1_address(call.sender);
        base.kind = retry_to.map_or(TxKind::Create, TxKind::Call);
        base.value = call.l2_call_value;
        base.data = call.data;
        base.nonce = 0;
        base.gas_price = retry_gas_price;
        base.gas_priority_fee = None;

        ArbitrumTxEnv::retryable_redeem(
            base,
            Some(submit_tx.ticket_id()),
            call.excess_fee_refund_address,
            source_tx.context.clone(),
        )
    }

    fn gas_estimate_target_tx(
        source_tx: &ArbitrumTxEnv,
        to: Address,
        contract_creation: bool,
        data: Bytes,
    ) -> ArbitrumTxEnv {
        let mut target_tx = source_tx.clone();
        target_tx.base.kind = if contract_creation {
            TxKind::Create
        } else {
            TxKind::Call(to)
        };
        target_tx.base.data = data;
        target_tx.retryable = None;
        target_tx
    }
}

impl<DB> ArbitrumApiImpl<DB>
where
    DB: BlockIndex + Sync + Send + 'static,
{
    pub(in crate::api_impl::arbitrum) fn try_execute_node_interface<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: &StateDB,
        tx: &ArbitrumTxEnv,
    ) -> Result<Option<ExecutionResult<HaltReason>>, EVMError<StateDB::Error, InvalidTransaction>>
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        let TxKind::Call(to) = tx.kind() else {
            return Ok(None);
        };
        if state.arbos_version() == 0 {
            return Ok(None);
        }
        if to == NODE_INTERFACE_DEBUG_ADDRESS {
            let data = tx.input().as_ref();
            return match INodeInterfaceDebugVirtual::INodeInterfaceDebugVirtualCalls::abi_decode(
                data,
            ) {
                Ok(INodeInterfaceDebugVirtual::INodeInterfaceDebugVirtualCalls::getRetryable(
                    call,
                )) => {
                    let info = state
                        .read_retryable_info(call.ticket)
                        .map_err(evm_custom_error::<StateDB>)?
                        .ok_or_else(|| {
                            evm_custom_error::<StateDB>(format!(
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
                    Ok(Some(Self::virtual_success(tx, ret.abi_encode().into())))
                }
                Err(err) => Err(evm_custom_error::<StateDB>(format!(
                    "invalid NodeInterfaceDebug calldata: {err}",
                ))),
            };
        }

        if to != NODE_INTERFACE_ADDRESS {
            return Ok(None);
        }

        let data = tx.input().as_ref();

        let output = match INodeInterfaceVirtual::INodeInterfaceVirtualCalls::abi_decode(data) {
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::gasEstimateComponents(call)) => {
                if call.to == NODE_INTERFACE_ADDRESS || call.to == NODE_INTERFACE_DEBUG_ADDRESS {
                    return Err(evm_custom_error::<StateDB>(
                        "cannot estimate virtual contract",
                    ));
                }

                let mut target_tx = Self::gas_estimate_target_tx(
                    tx,
                    call.to,
                    call.contractCreation,
                    call.data.clone(),
                );
                let execution_gas = self.estimate_node_interface_execution_gas(
                    block_env,
                    state,
                    target_tx.clone(),
                )?;
                target_tx.base.gas_limit = execution_gas;
                let pricing = state.read_pricing();
                let l1_gas = pricing
                    .as_ref()
                    .filter(|_| block_env.basefee != 0)
                    .map(|pricing| pricing.poster_gas(&target_tx.base, block_env.basefee))
                    .unwrap_or_default();
                let l1_base_fee = pricing
                    .as_ref()
                    .map(|pricing| pricing.price_per_unit)
                    .unwrap_or_default();
                (
                    execution_gas,
                    l1_gas,
                    U256::from(block_env.basefee),
                    l1_base_fee,
                )
                    .abi_encode()
                    .into()
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::gasEstimateL1Component(call)) => {
                let mut target_tx = Self::gas_estimate_target_tx(
                    tx,
                    call.to,
                    call.contractCreation,
                    call.data.clone(),
                );
                target_tx.base.gas_limit = random_gas_for_l1_component();
                let pricing = state.read_pricing();
                let l1_gas = pricing
                    .as_ref()
                    .filter(|_| block_env.basefee != 0)
                    .map(|pricing| {
                        pricing.gas_estimate_l1_component(&target_tx.base, block_env.basefee)
                    })
                    .unwrap_or_default();
                let l1_base_fee = pricing
                    .as_ref()
                    .map(|pricing| pricing.price_per_unit)
                    .unwrap_or_default();
                (l1_gas, U256::from(block_env.basefee), l1_base_fee)
                    .abi_encode()
                    .into()
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::nitroGenesisBlock(_)) => {
                configured_nitro_genesis_block_num(
                    self.evm_cfg.cfg.chain_id,
                    self.evm_cfg.custom_cfg.as_ref(),
                )
                .abi_encode()
                .into()
            }
            Ok(
                INodeInterfaceVirtual::INodeInterfaceVirtualCalls::legacyLookupMessageBatchProof(_),
            ) => {
                return Err(evm_custom_error::<StateDB>(
                    "this node doesnt support classicLookupMessageBatchProof",
                ))
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::estimateRetryableTicket(
                call,
            )) => {
                let target_tx = Self::retryable_redeem_tx(tx, block_env, state, call.into());
                match self.transact_evm(block_env, BorrowedState(state), target_tx)? {
                    ExecutionResult::Success { output, .. } => output.into_data().0.into(),
                    ExecutionResult::Revert { output, .. } => {
                        return Err(evm_custom_error::<StateDB>(format!(
                            "Reverted: {:?}",
                            decode_revert_reason(&output).unwrap_or("execution revert".to_string())
                        )))
                    }
                    ExecutionResult::Halt { reason, .. } => {
                        return Err(evm_custom_error::<StateDB>(format!("Halted: {:?}", reason)))
                    }
                }
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::blockL1Num(call)) => {
                rpc_to_evm_result::<StateDB, _>(block_l1_num(
                    &self.db,
                    call.l2BlockNum,
                    configured_legacy_zero_base_fee_until(self.evm_cfg.custom_cfg.as_ref()),
                ))?
                .abi_encode()
                .into()
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::l2BlockRangeForL1(call)) => {
                let genesis_block_num = configured_nitro_genesis_block_num(
                    self.evm_cfg.cfg.chain_id,
                    self.evm_cfg.custom_cfg.as_ref(),
                );
                let legacy_zero_base_fee_until =
                    configured_legacy_zero_base_fee_until(self.evm_cfg.custom_cfg.as_ref());
                let current_block_num =
                    rpc_to_evm_result::<StateDB, _>(latest_l2_block_num(&self.db))?;
                let (first_block, last_block) =
                    rpc_to_evm_result::<StateDB, _>(l2_block_range_for_l1(
                        &self.db,
                        genesis_block_num,
                        current_block_num,
                        call.blockNum,
                        legacy_zero_base_fee_until,
                    ))?;
                (first_block, last_block).abi_encode().into()
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::getL1Confirmations(_)) => {
                return Err(evm_custom_error::<StateDB>(
                    "NodeInterface.getL1Confirmations requires node backend context",
                ))
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::constructOutboxProof(_)) => {
                return Err(evm_custom_error::<StateDB>(
                    "NodeInterface.constructOutboxProof requires node backend context",
                ))
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::findBatchContainingBlock(_)) => {
                return Err(evm_custom_error::<StateDB>(
                    "NodeInterface.findBatchContainingBlock requires node backend context",
                ))
            }
            Err(err) => {
                return Err(evm_custom_error::<StateDB>(format!(
                    "invalid NodeInterface calldata: {err}",
                )))
            }
        };

        Ok(Some(Self::virtual_success(tx, output)))
    }

    fn estimate_node_interface_execution_gas<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: &StateDB,
        mut tx: ArbitrumTxEnv,
    ) -> Result<u64, EVMError<StateDB::Error, InvalidTransaction>>
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        let max_gas_limit = self
            .evm_cfg
            .cfg
            .tx_gas_limit_cap
            .map_or_else(|| block_env.gas_limit, |cap| cap.min(block_env.gas_limit));
        let mut highest_gas_limit = tx.gas_limit().max(max_gas_limit);

        if tx.input().is_empty() {
            if let TxKind::Call(to) = tx.kind() {
                let no_code_callee = state
                    .basic_ref(to)?
                    .map(|account| account.is_empty_code_hash() || account.code_hash().is_zero())
                    .unwrap_or(true);
                if no_code_callee {
                    let mut min_tx = tx.clone();
                    min_tx.base.gas_limit = MIN_TRANSACTION_GAS;
                    if let Ok(exec_res) = self.transact_evm(block_env, BorrowedState(state), min_tx)
                    {
                        if exec_res.is_success() {
                            return Ok(MIN_TRANSACTION_GAS);
                        }
                    }
                }
            }
        }

        if tx.gas_price() > 0 {
            let caller = state.basic_ref(tx.caller())?;
            let balance = caller
                .map(|account| account.balance)
                .unwrap_or_default()
                .saturating_sub(tx.value());
            let gas_allowance = balance
                .checked_div(U256::from(tx.gas_price()))
                .unwrap_or_default()
                .try_into()
                .unwrap_or(u64::MAX);
            highest_gas_limit = highest_gas_limit.min(gas_allowance);
        }

        tx.base.gas_limit = tx.gas_limit().min(highest_gas_limit);
        let res = self.transact_evm(block_env, BorrowedState(state), tx.clone())?;
        let gas_refund = match &res {
            ExecutionResult::Success { gas, .. } => gas.inner_refunded(),
            ExecutionResult::Halt { reason, .. } => {
                return Err(evm_custom_error::<StateDB>(format!("Halted: {:?}", reason)));
            }
            ExecutionResult::Revert { output, .. } => {
                return Err(evm_custom_error::<StateDB>(format!(
                    "Reverted: {:?}",
                    decode_revert_reason(output).unwrap_or("execution revert".to_string())
                )));
            }
        };

        highest_gas_limit = tx.gas_limit();
        let gas_used = res.gas_used();
        let mut lowest_gas_limit = gas_used.saturating_sub(1);
        let optimistic_gas_limit = ((gas_used as u128)
            .saturating_add(gas_refund as u128)
            .saturating_add(CALL_STIPEND_GAS as u128)
            .saturating_mul(64)
            / 63)
            .min(u64::MAX as u128) as u64;

        if optimistic_gas_limit < highest_gas_limit {
            tx.base.gas_limit = optimistic_gas_limit;
            let res = self.transact_evm(block_env, BorrowedState(state), tx.clone())?;
            update_estimated_gas_range::<StateDB>(
                &res,
                optimistic_gas_limit,
                &mut highest_gas_limit,
                &mut lowest_gas_limit,
            )?;
        }

        loop {
            let gas_limit_range = highest_gas_limit.saturating_sub(lowest_gas_limit);
            if gas_limit_range <= 1 {
                break;
            }
            if gas_limit_range as f64 / (highest_gas_limit as f64) < ESTIMATE_GAS_ERROR_RATIO {
                break;
            }

            let mut mid_gas_limit = lowest_gas_limit + (highest_gas_limit - lowest_gas_limit) / 2;
            if mid_gas_limit > lowest_gas_limit.saturating_mul(2) {
                mid_gas_limit = lowest_gas_limit.saturating_mul(2);
            }
            tx.base.gas_limit = mid_gas_limit;
            match self.transact_evm(block_env, BorrowedState(state), tx.clone()) {
                Ok(res) => update_estimated_gas_range::<StateDB>(
                    &res,
                    mid_gas_limit,
                    &mut highest_gas_limit,
                    &mut lowest_gas_limit,
                )?,
                Err(err) => match err {
                    EVMError::Transaction(
                        InvalidTransaction::CallerGasLimitMoreThanBlock
                        | InvalidTransaction::TxGasLimitGreaterThanCap { .. },
                    ) => {
                        highest_gas_limit = mid_gas_limit;
                    }
                    EVMError::Transaction(
                        InvalidTransaction::CallGasCostMoreThanGasLimit { .. }
                        | InvalidTransaction::GasFloorMoreThanGasLimit { .. },
                    ) => {
                        lowest_gas_limit = mid_gas_limit;
                    }
                    err => return Err(err),
                },
            }
        }

        let buffer = self.evm_cfg.estimate_gas_buffer;
        if buffer > 100 {
            let buffered = (highest_gas_limit as u128 * buffer as u128) / 100;
            Ok(buffered.min(u64::MAX as u128) as u64)
        } else {
            Ok(highest_gas_limit)
        }
    }
}

fn evm_custom_error<StateDB>(
    message: impl Into<String>,
) -> EVMError<<StateDB as DatabaseRef>::Error, InvalidTransaction>
where
    StateDB: DatabaseRef,
{
    EVMError::Custom(message.into())
}

fn rpc_to_evm_result<StateDB, T>(
    result: RpcResult<T>,
) -> Result<T, EVMError<<StateDB as DatabaseRef>::Error, InvalidTransaction>>
where
    StateDB: DatabaseRef,
{
    result.map_err(|err| evm_custom_error::<StateDB>(err.message().to_string()))
}

fn update_estimated_gas_range<StateDB>(
    result: &ExecutionResult<HaltReason>,
    tx_gas_limit: u64,
    highest_gas_limit: &mut u64,
    lowest_gas_limit: &mut u64,
) -> Result<(), EVMError<<StateDB as DatabaseRef>::Error, InvalidTransaction>>
where
    StateDB: DatabaseRef,
{
    match result {
        ExecutionResult::Success { .. } => {
            *highest_gas_limit = tx_gas_limit;
        }
        ExecutionResult::Revert { .. } => {
            *lowest_gas_limit = tx_gas_limit;
        }
        ExecutionResult::Halt { reason, .. } => match reason {
            HaltReason::OutOfGas(_) | HaltReason::InvalidFEOpcode => {
                *lowest_gas_limit = tx_gas_limit;
            }
            reason => return Err(evm_custom_error::<StateDB>(format!("Halted: {:?}", reason))),
        },
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_chains::arbitrum::tx::ArbitrumTxContext;
    use revm::context::TxEnv;
    use revm::database::EmptyDB;

    #[test]
    fn retryable_redeem_tx_matches_nitro_message_swap_fields() {
        let sender = Address::ZERO;
        let target = Address::with_last_byte(0xaa);
        let fee_refund = Address::with_last_byte(0xbb);
        let call_value_refund = Address::with_last_byte(0xcc);
        let data = Bytes::from_static(&[1, 2, 3, 4]);
        let call = RetryableRedeemCall {
            sender,
            deposit: U256::from(1_000_000u64),
            to: target,
            l2_call_value: U256::from(7u64),
            excess_fee_refund_address: fee_refund,
            call_value_refund_address: call_value_refund,
            data: data.clone(),
        };

        let mut block_env = BlockEnv::default();
        block_env.basefee = 42;
        let source_tx = ArbitrumTxEnv::new(
            TxEnv {
                kind: TxKind::Call(NODE_INTERFACE_ADDRESS),
                gas_limit: 777_000,
                gas_price: 999,
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );
        let target_tx = ArbitrumApiImpl::<()>::retryable_redeem_tx(
            &source_tx,
            &block_env,
            &EmptyDB::default(),
            call.clone(),
        );

        assert_eq!(
            target_tx.caller(),
            alloy::primitives::address!("1111000000000000000000000000000000001111")
        );
        assert_eq!(target_tx.kind(), TxKind::Call(target));
        assert_eq!(target_tx.value(), U256::from(7u64));
        assert_eq!(target_tx.input().as_ref(), data.as_ref());
        assert_eq!(target_tx.gas_limit(), 777_000);
        assert_eq!(target_tx.gas_price(), 42);
        assert_eq!(
            target_tx.retryable.as_ref().map(|ctx| ctx.refund_to),
            Some(fee_refund)
        );
    }

    #[test]
    fn retryable_redeem_call_adapter_maps_all_fields() {
        let fee_refund = Address::with_last_byte(0xbb);
        let call_value_refund = Address::with_last_byte(0xcc);
        let target = Address::with_last_byte(0xaa);
        let call = INodeInterfaceVirtual::estimateRetryableTicketCall {
            sender: Address::ZERO,
            deposit: U256::from(1_000_000u64),
            to: target,
            l2CallValue: U256::from(7u64),
            excessFeeRefundAddress: fee_refund,
            callValueRefundAddress: call_value_refund,
            data: Bytes::from_static(&[1, 2, 3, 4]),
        };

        let decoded_call = RetryableRedeemCall::from(call.clone());
        let refund_to = decoded_call.excess_fee_refund_address;
        assert_eq!(decoded_call.sender, call.sender);
        assert_eq!(decoded_call.deposit, call.deposit);
        assert_eq!(decoded_call.to, call.to);
        assert_eq!(decoded_call.l2_call_value, call.l2CallValue);
        assert_eq!(
            decoded_call.excess_fee_refund_address,
            call.excessFeeRefundAddress
        );
        assert_eq!(
            decoded_call.call_value_refund_address,
            call.callValueRefundAddress
        );
        assert_eq!(decoded_call.data, call.data);

        assert_eq!(refund_to, fee_refund);
        assert_eq!(decoded_call.to, target);
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
    fn arbitrum_one_nitro_genesis_block_matches_node_interface() {
        assert_eq!(configured_nitro_genesis_block_num(42161, None), 22_207_817);
    }

    #[test]
    fn retryable_redeem_tx_uses_create_for_zero_target() {
        let call = RetryableRedeemCall {
            sender: Address::ZERO,
            deposit: U256::ZERO,
            to: Address::ZERO,
            l2_call_value: U256::ZERO,
            excess_fee_refund_address: Address::ZERO,
            call_value_refund_address: Address::ZERO,
            data: Bytes::new(),
        };

        let target_tx = ArbitrumApiImpl::<()>::retryable_redeem_tx(
            &ArbitrumTxEnv::new(TxEnv::default(), ArbitrumTxContext::default()),
            &BlockEnv::default(),
            &EmptyDB::default(),
            call,
        );

        assert_eq!(target_tx.kind(), TxKind::Create);
        assert_eq!(target_tx.gas_price(), 0);
    }
}
