use crate::api_impl::arbitrum::api::ArbitrumApiImpl;
use crate::error::internal_rpc_err;
use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::sol_types::{decode_revert_reason, SolInterface, SolValue};
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arbitrum::arbos_state::{ArbStateReader, ARBOS_STATE_ADDRESS};
use leafage_evm_chains::arbitrum::precompile::{
    NODE_INTERFACE_ADDRESS, NODE_INTERFACE_DEBUG_ADDRESS,
};
use leafage_evm_chains::arbitrum::tx::{
    nitro_message_gas_price, ArbitrumSubmitRetryableTx, ArbitrumTxEnv,
};
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
const COPY_GAS: u64 = 3;
const STORAGE_READ_GAS: u64 = 800;
const NODE_INTERFACE_CONTEXT_STORAGE_READS: u64 = 1;
const NODE_INTERFACE_GAS_ESTIMATE_COMPONENTS_STORAGE_READS: u64 = 5;
const NODE_INTERFACE_GAS_ESTIMATE_L1_COMPONENT_STORAGE_READS: u64 = 4;
const L2_PRICING_SUBSPACE: &[u8] = &[1];
const L2_BASE_FEE_WEI_OFFSET: u64 = 2;

fn random_gas_for_l1_component() -> u64 {
    u32::from_be_bytes(
        keccak256("Gas").as_slice()[..4]
            .try_into()
            .expect("fixed-size keccak prefix"),
    ) as u64
}

fn copy_gas(byte_count: usize) -> u64 {
    COPY_GAS.saturating_mul((byte_count as u64).div_ceil(32))
}

fn arbos_child_key(parent_key: &[u8], id: &[u8]) -> [u8; 32] {
    keccak256([parent_key, id].concat()).0
}

fn arbos_slot_for_key(storage_key: &[u8], key: [u8; 32]) -> U256 {
    let mut input = Vec::with_capacity(storage_key.len() + 31);
    input.extend_from_slice(storage_key);
    input.extend_from_slice(&key[..31]);
    let hashed = keccak256(&input).0;
    let mut slot = [0u8; 32];
    slot[..31].copy_from_slice(&hashed[..31]);
    slot[31] = key[31];
    U256::from_be_bytes::<32>(slot)
}

fn arbos_slot_at(storage_key: &[u8], offset: u64) -> U256 {
    arbos_slot_for_key(storage_key, U256::from(offset).to_be_bytes())
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

#[derive(Debug)]
pub(in crate::api_impl::arbitrum) enum NodeInterfaceAction {
    Return(ExecutionResult<HaltReason>),
    Replace(ArbitrumTxEnv),
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
        .unwrap_or(match chain_id {
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

    fn virtual_success<StateDB>(
        tx: &ArbitrumTxEnv,
        output: Bytes,
        gas_used: u64,
    ) -> Result<ExecutionResult<HaltReason>, EVMError<StateDB::Error, InvalidTransaction>>
    where
        StateDB: DatabaseRef,
    {
        if tx.gas_limit() < gas_used {
            return Err(EVMError::Transaction(
                InvalidTransaction::CallGasCostMoreThanGasLimit {
                    initial_gas: gas_used,
                    gas_limit: tx.gas_limit(),
                },
            ));
        }

        Ok(ExecutionResult::Success {
            reason: SuccessReason::Return,
            gas: ResultGas::new(tx.gas_limit(), gas_used, 0, 0, 0),
            logs: Vec::new(),
            output: Output::Call(output),
        })
    }

    fn virtual_call_gas(data: &[u8], output: &[u8], method_storage_reads: u64) -> u64 {
        let storage_reads = NODE_INTERFACE_CONTEXT_STORAGE_READS
            .saturating_add(method_storage_reads);
        copy_gas(data.len().saturating_sub(4))
            .saturating_add(copy_gas(output.len()))
            .saturating_add(STORAGE_READ_GAS.saturating_mul(storage_reads))
    }

    fn retryable_info_storage_reads(data_len: usize) -> u64 {
        // Retryable scalar fields, calldata length, and calldata words. The
        // OpenArbosState read is added by virtual_call_gas.
        10u64.saturating_add((data_len as u64) / 32)
    }

    fn l2_basefee_from_state<StateDB>(state: &StateDB) -> Result<u64, StateDB::Error>
    where
        StateDB: DatabaseRef,
    {
        let l2_pricing_key = arbos_child_key(&[], L2_PRICING_SUBSPACE);
        state
            .storage_ref(
                ARBOS_STATE_ADDRESS,
                arbos_slot_at(&l2_pricing_key, L2_BASE_FEE_WEI_OFFSET),
            )
            .map(|basefee| basefee.wrapping_to::<u64>())
    }

    fn retryable_submission<StateDB>(
        source_tx: &ArbitrumTxEnv,
        block_env: &BlockEnv,
        state: &StateDB,
        call: &RetryableRedeemCall,
    ) -> Result<ArbitrumSubmitRetryableTx, StateDB::Error>
    where
        StateDB: DatabaseRef,
    {
        let source_message_gas_price =
            nitro_message_gas_price(source_tx, block_env.basefee as u128);
        let l1_base_fee = state
            .try_read_pricing()?
            .map(|pricing| pricing.price_per_unit)
            .unwrap_or_default();
        let max_submission_fee =
            ArbitrumSubmitRetryableTx::submission_fee(call.data.len(), l1_base_fee);
        let retry_to = (call.to != Address::ZERO).then_some(call.to);

        Ok(ArbitrumSubmitRetryableTx {
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
        })
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
        target_tx.variant = Default::default();
        target_tx
    }
}

impl<DB> ArbitrumApiImpl<DB> {
    pub(in crate::api_impl::arbitrum) fn node_interface_action<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: &StateDB,
        tx: &ArbitrumTxEnv,
    ) -> Result<Option<NodeInterfaceAction>, EVMError<StateDB::Error, InvalidTransaction>>
    where
        DB: BlockIndex + Sync + Send + 'static,
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        let TxKind::Call(to) = tx.kind() else {
            return Ok(None);
        };
        if to != NODE_INTERFACE_ADDRESS && to != NODE_INTERFACE_DEBUG_ADDRESS {
            return Ok(None);
        }
        if state.try_arbos_version().map_err(EVMError::Database)? == 0 {
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
                    let retryable_data_len = ret.data.len();
                    let output: Bytes = ret.abi_encode().into();
                    let gas_used = Self::virtual_call_gas(
                        data,
                        output.as_ref(),
                        Self::retryable_info_storage_reads(retryable_data_len),
                    );
                    let result = Self::virtual_success::<StateDB>(tx, output, gas_used)?;
                    Ok(Some(NodeInterfaceAction::Return(result)))
                }
                Err(err) => Err(evm_custom_error::<StateDB>(format!(
                    "invalid NodeInterfaceDebug calldata: {err}",
                ))),
            };
        }

        let data = tx.input().as_ref();

        let (output, gas_used) = match INodeInterfaceVirtual::INodeInterfaceVirtualCalls::abi_decode(
            data,
        ) {
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
                let l2_basefee = Self::l2_basefee_from_state(state).map_err(EVMError::Database)?;
                let execution_gas = self.estimate_node_interface_execution_gas(
                    block_env,
                    state,
                    target_tx.clone(),
                )?;
                target_tx.base.gas_limit = execution_gas;
                let pricing = state.try_read_pricing().map_err(EVMError::Database)?;
                let l1_gas = pricing
                    .as_ref()
                    .filter(|_| l2_basefee != 0)
                    .map(|pricing| pricing.poster_gas(&target_tx.base, l2_basefee))
                    .unwrap_or_default();
                let l1_base_fee = pricing
                    .as_ref()
                    .map(|pricing| pricing.price_per_unit)
                    .unwrap_or_default();
                let output: Bytes = (execution_gas, l1_gas, U256::from(l2_basefee), l1_base_fee)
                    .abi_encode()
                    .into();
                let gas_used = Self::virtual_call_gas(
                    data,
                    output.as_ref(),
                    NODE_INTERFACE_GAS_ESTIMATE_COMPONENTS_STORAGE_READS,
                );
                (output, gas_used)
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::gasEstimateL1Component(call)) => {
                let mut target_tx = Self::gas_estimate_target_tx(
                    tx,
                    call.to,
                    call.contractCreation,
                    call.data.clone(),
                );
                target_tx.base.gas_limit = random_gas_for_l1_component();
                let l2_basefee = Self::l2_basefee_from_state(state).map_err(EVMError::Database)?;
                let pricing = state.try_read_pricing().map_err(EVMError::Database)?;
                let l1_gas = pricing
                    .as_ref()
                    .filter(|_| l2_basefee != 0)
                    .map(|pricing| pricing.gas_estimate_l1_component(&target_tx.base, l2_basefee))
                    .unwrap_or_default();
                let l1_base_fee = pricing
                    .as_ref()
                    .map(|pricing| pricing.price_per_unit)
                    .unwrap_or_default();
                let output: Bytes = (l1_gas, U256::from(l2_basefee), l1_base_fee)
                    .abi_encode()
                    .into();
                let gas_used = Self::virtual_call_gas(
                    data,
                    output.as_ref(),
                    NODE_INTERFACE_GAS_ESTIMATE_L1_COMPONENT_STORAGE_READS,
                );
                (output, gas_used)
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::nitroGenesisBlock(_)) => {
                let output: Bytes = configured_nitro_genesis_block_num(
                    self.evm_cfg.cfg.chain_id,
                    self.evm_cfg.custom_cfg.as_ref(),
                )
                .abi_encode()
                .into();
                let gas_used = copy_gas(data.len().saturating_sub(4))
                    .saturating_add(copy_gas(output.len()));
                (output, gas_used)
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
                let call = RetryableRedeemCall::from(call);
                let submit_tx = Self::retryable_submission(tx, block_env, state, &call)
                    .map_err(EVMError::Database)?;
                return Ok(Some(NodeInterfaceAction::Replace(
                    ArbitrumTxEnv::submit_retryable(tx.clone(), submit_tx),
                )));
            }
            Ok(INodeInterfaceVirtual::INodeInterfaceVirtualCalls::blockL1Num(call)) => {
                let output: Bytes = rpc_to_evm_result::<StateDB, _>(block_l1_num(
                    &self.db,
                    call.l2BlockNum,
                    configured_legacy_zero_base_fee_until(self.evm_cfg.custom_cfg.as_ref()),
                ))?
                .abi_encode()
                .into();
                let gas_used = Self::virtual_call_gas(data, output.as_ref(), 0);
                (output, gas_used)
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
                let output: Bytes = (first_block, last_block).abi_encode().into();
                let gas_used = Self::virtual_call_gas(data, output.as_ref(), 0);
                (output, gas_used)
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

        let result = Self::virtual_success::<StateDB>(tx, output, gas_used)?;
        Ok(Some(NodeInterfaceAction::Return(result)))
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
        // Nitro runs gasEstimateComponents' target message in gas-estimation
        // mode, so the executions here charge the padded L1 poster gas.
        tx.context.gas_estimation = true;
        let max_gas_limit = self
            .evm_cfg
            .cfg
            .tx_gas_limit_cap
            .map_or_else(|| block_env.gas_limit, |cap| cap.min(block_env.gas_limit));
        let mut highest_gas_limit = max_gas_limit;

        if tx.input().is_empty() {
            if let TxKind::Call(to) = tx.kind() {
                let no_code_callee = state
                    .basic_ref(to)?
                    .map(|account| account.is_empty_code_hash() || account.code_hash().is_zero())
                    .unwrap_or(true);
                if no_code_callee {
                    let mut min_tx = tx.clone();
                    min_tx.base.gas_limit = MIN_TRANSACTION_GAS;
                    if let Ok((exec_res, _)) =
                        self.transact_arbitrum(block_env, BorrowedState(state), min_tx)
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
        let gas_limit_cap = highest_gas_limit;

        tx.base.gas_limit = highest_gas_limit;
        let (res, _) = self.transact_arbitrum(block_env, BorrowedState(state), tx.clone())?;
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
            let (res, _) = self.transact_arbitrum(block_env, BorrowedState(state), tx.clone())?;
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
            match self.transact_arbitrum(block_env, BorrowedState(state), tx.clone()) {
                Ok((res, _)) => update_estimated_gas_range::<StateDB>(
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
            Ok((buffered.min(u64::MAX as u128) as u64).min(gas_limit_cap))
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
    use alloy::sol_types::SolCall;
    use leafage_evm_chains::arbitrum::tx::ArbitrumTxContext;
    use leafage_evm_chains::arbitrum::ArbitrumHardfork;
    use leafage_evm_types::CfgEnv;
    use revm::context::TxEnv;
    use revm::context_interface::transaction::TransactionType;
    use revm::database::{CacheDB, EmptyDB};

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct ExpectedDbError;

    impl core::fmt::Display for ExpectedDbError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("expected database error")
        }
    }

    impl std::error::Error for ExpectedDbError {}
    impl revm::database_interface::DBErrorMarker for ExpectedDbError {}

    #[derive(Debug)]
    struct FailingState;

    impl DatabaseRef for FailingState {
        type Error = ExpectedDbError;

        fn basic_ref(&self, _: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(ExpectedDbError)
        }

        fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, Self::Error> {
            Err(ExpectedDbError)
        }

        fn storage_ref(&self, _: Address, _: U256) -> Result<U256, Self::Error> {
            Err(ExpectedDbError)
        }

        fn block_hash_ref(&self, _: u64) -> Result<B256, Self::Error> {
            Err(ExpectedDbError)
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct TestBlockIndex;

    impl BlockIndex for TestBlockIndex {
        type Error = core::convert::Infallible;

        fn get_block_by_id(&self, _: BlockId) -> Result<Option<BlockInfo>, Self::Error> {
            Ok(None)
        }
    }

    fn test_api() -> ArbitrumApiImpl<TestBlockIndex> {
        let mut cfg = CfgEnv::new_with_spec(ArbitrumHardfork::Prague);
        cfg.chain_id = 42161;
        ArbitrumApiImpl::new(
            TestBlockIndex,
            cfg,
            None,
            None,
            None,
            None,
            true,
            false,
            "test".to_string(),
            100,
            None,
            None,
        )
    }

    fn state_with_l2_basefee(basefee: u64) -> CacheDB<EmptyDB> {
        const ARBOS_VERSION_OFFSET: u64 = 0;
        const L2_PER_TX_GAS_LIMIT_OFFSET: u64 = 7;

        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(ARBOS_STATE_ADDRESS, AccountInfo::default());
        let l2_pricing_key = arbos_child_key(&[], L2_PRICING_SUBSPACE);
        db.insert_account_storage(
            ARBOS_STATE_ADDRESS,
            arbos_slot_at(&[], ARBOS_VERSION_OFFSET),
            U256::from(51),
        )
        .expect("write ArbOS version");
        db.insert_account_storage(
            ARBOS_STATE_ADDRESS,
            arbos_slot_at(&l2_pricing_key, L2_PER_TX_GAS_LIMIT_OFFSET),
            U256::from(32_000_000u64),
        )
        .expect("write per-transaction gas limit");
        db.insert_account_storage(
            ARBOS_STATE_ADDRESS,
            arbos_slot_at(&l2_pricing_key, L2_BASE_FEE_WEI_OFFSET),
            U256::from(basefee),
        )
        .expect("write L2 base fee");
        db
    }

    #[test]
    fn node_interface_version_read_preserves_database_error() {
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                kind: TxKind::Call(NODE_INTERFACE_ADDRESS),
                ..Default::default()
            },
            Default::default(),
        );

        let error = test_api()
            .node_interface_action(&BlockEnv::default(), &FailingState, &tx)
            .expect_err("NodeInterface version failure must be returned");
        assert!(matches!(error, EVMError::Database(ExpectedDbError)));
    }

    #[test]
    fn ordinary_call_does_not_read_node_interface_version() {
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                kind: TxKind::Call(Address::with_last_byte(0x42)),
                ..Default::default()
            },
            Default::default(),
        );

        let result = test_api()
            .node_interface_action(&BlockEnv::default(), &FailingState, &tx)
            .expect("ordinary calls must bypass NodeInterface probing");
        assert!(result.is_none());
    }

    #[test]
    fn retryable_submission_matches_nitro_message_swap_fields() {
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

        let block_env = BlockEnv {
            basefee: 42,
            ..Default::default()
        };
        let source_tx = ArbitrumTxEnv::new(
            TxEnv {
                kind: TxKind::Call(NODE_INTERFACE_ADDRESS),
                gas_limit: 777_000,
                gas_price: 999,
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );
        let submit_tx = ArbitrumApiImpl::<()>::retryable_submission(
            &source_tx,
            &block_env,
            &EmptyDB::default(),
            &call,
        )
        .expect("retryable submission");

        assert_eq!(
            submit_tx.from,
            alloy::primitives::address!("1111000000000000000000000000000000001111")
        );
        assert_eq!(submit_tx.retry_to, Some(target));
        assert_eq!(submit_tx.retry_value, U256::from(7u64));
        assert_eq!(submit_tx.retry_data.as_ref(), data.as_ref());
        assert_eq!(submit_tx.gas, 777_000);
        assert_eq!(submit_tx.gas_fee_cap, U256::from(999u64));
        assert_eq!(submit_tx.fee_refund_addr, fee_refund);
        assert_eq!(submit_tx.beneficiary, call_value_refund);
    }

    #[test]
    fn retryable_submission_defaults_missing_eip1559_tip_to_zero() {
        let call = RetryableRedeemCall {
            sender: Address::ZERO,
            deposit: U256::from(1_000_000u64),
            to: Address::with_last_byte(0xaa),
            l2_call_value: U256::ZERO,
            excess_fee_refund_address: Address::with_last_byte(0xbb),
            call_value_refund_address: Address::with_last_byte(0xcc),
            data: Bytes::new(),
        };
        let block_env = BlockEnv {
            basefee: 42,
            ..Default::default()
        };
        let source_tx = ArbitrumTxEnv::new(
            TxEnv {
                kind: TxKind::Call(NODE_INTERFACE_ADDRESS),
                tx_type: TransactionType::Eip1559 as u8,
                gas_limit: 777_000,
                gas_price: 999,
                gas_priority_fee: None,
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );

        let submit_tx = ArbitrumApiImpl::<()>::retryable_submission(
            &source_tx,
            &block_env,
            &EmptyDB::default(),
            &call,
        )
        .expect("retryable submission");

        assert_eq!(submit_tx.gas_fee_cap, U256::from(block_env.basefee));
    }

    #[test]
    fn retryable_call_adapter_maps_fields() {
        let data = Bytes::from_static(&[1, 2, 3, 4]);
        let call = INodeInterfaceVirtual::estimateRetryableTicketCall {
            sender: Address::with_last_byte(1),
            deposit: U256::from(1_000_000u64),
            to: Address::with_last_byte(2),
            l2CallValue: U256::from(7u64),
            excessFeeRefundAddress: Address::with_last_byte(3),
            callValueRefundAddress: Address::with_last_byte(4),
            data: data.clone(),
        };

        let decoded = RetryableRedeemCall::from(call.clone());

        assert_eq!(decoded.sender, call.sender);
        assert_eq!(decoded.deposit, call.deposit);
        assert_eq!(decoded.to, call.to);
        assert_eq!(decoded.l2_call_value, call.l2CallValue);
        assert_eq!(
            decoded.excess_fee_refund_address,
            call.excessFeeRefundAddress
        );
        assert_eq!(
            decoded.call_value_refund_address,
            call.callValueRefundAddress
        );
        assert_eq!(decoded.data, data);
    }

    #[test]
    fn estimate_retryable_ticket_replaces_the_message() {
        let sender = Address::with_last_byte(1);
        let target = Address::with_last_byte(2);
        let call = INodeInterfaceVirtual::estimateRetryableTicketCall {
            sender,
            deposit: U256::from(1_000_000u64),
            to: target,
            l2CallValue: U256::from(7u64),
            excessFeeRefundAddress: Address::with_last_byte(3),
            callValueRefundAddress: Address::with_last_byte(4),
            data: Bytes::from_static(&[1, 2, 3, 4]),
        };
        let source = ArbitrumTxEnv::new(
            TxEnv {
                kind: TxKind::Call(NODE_INTERFACE_ADDRESS),
                gas_limit: 100_000,
                data: call.abi_encode().into(),
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );
        let state = state_with_l2_basefee(0);

        let action = test_api()
            .node_interface_action(&BlockEnv::default(), &state, &source)
            .expect("NodeInterface call should decode")
            .expect("NodeInterface call should be intercepted");
        let NodeInterfaceAction::Replace(replacement) = action else {
            panic!("estimateRetryableTicket must replace the source message");
        };
        let submit = replacement
            .submit_retryable_tx()
            .expect("replacement must carry a submit-retryable transaction");

        assert_eq!(submit.retry_to, Some(target));
        assert_eq!(submit.retry_value, U256::from(7u64));
        assert_eq!(submit.gas, source.gas_limit());
        assert_eq!(replacement.kind(), TxKind::Call(target));
        assert_eq!(replacement.input(), &call.data);
    }

    #[test]
    fn node_interface_execution_estimate_uses_cap_not_virtual_call_gas() {
        let api = test_api();
        let block_env = BlockEnv {
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                caller: Address::with_last_byte(1),
                chain_id: Some(42161),
                gas_limit: 10,
                kind: TxKind::Call(Address::with_last_byte(2)),
                data: Bytes::from_static(&[1, 2, 3, 4]),
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );

        let state = state_with_l2_basefee(0);
        let gas = api
            .estimate_node_interface_execution_gas(&block_env, &state, tx)
            .expect("target gas estimation should not inherit source gas limit");

        assert!(gas > MIN_TRANSACTION_GAS);
        assert!(gas < block_env.gas_limit);
    }

    #[test]
    fn node_interface_execution_estimate_does_not_exceed_configured_cap() {
        let mut api = test_api();
        api.evm_cfg.cfg.tx_gas_limit_cap = Some(500_000);
        let block_env = BlockEnv {
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                caller: Address::with_last_byte(1),
                chain_id: Some(42161),
                gas_limit: 2_000_000,
                kind: TxKind::Call(Address::with_last_byte(2)),
                data: Bytes::from_static(&[1, 2, 3, 4]),
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );

        let state = state_with_l2_basefee(0);
        let gas = api
            .estimate_node_interface_execution_gas(&block_env, &state, tx)
            .expect("source gas above cap should not break target gas estimation");

        assert!(gas > MIN_TRANSACTION_GAS);
        assert!(gas <= api.evm_cfg.cfg.tx_gas_limit_cap.unwrap());
    }

    #[test]
    fn node_interface_execution_estimate_buffer_is_clamped_to_cap() {
        let mut api = test_api();
        api.evm_cfg.cfg.tx_gas_limit_cap = Some(30_000);
        api.evm_cfg.estimate_gas_buffer = 200;
        let block_env = BlockEnv {
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                caller: Address::with_last_byte(1),
                chain_id: Some(42161),
                gas_limit: 2_000_000,
                kind: TxKind::Call(Address::with_last_byte(2)),
                data: Bytes::from_static(&[1, 2, 3, 4]),
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );

        let state = state_with_l2_basefee(0);
        let gas = api
            .estimate_node_interface_execution_gas(&block_env, &state, tx)
            .expect("buffered estimate should still obey the cap");

        assert!(gas <= api.evm_cfg.cfg.tx_gas_limit_cap.unwrap());
    }

    #[test]
    fn virtual_call_gas_charges_copy_and_storage_reads() {
        let call = INodeInterfaceVirtual::gasEstimateComponentsCall {
            to: Address::with_last_byte(0xaa),
            contractCreation: false,
            data: Bytes::from_static(&[0x95, 0xd8, 0x9b, 0x41]),
        };
        let data = call.abi_encode();
        let output = (1u64, 2u64, U256::from(3), U256::from(4)).abi_encode();

        assert_eq!(
            ArbitrumApiImpl::<()>::virtual_call_gas(
                &data,
                &output,
                NODE_INTERFACE_GAS_ESTIMATE_COMPONENTS_STORAGE_READS,
            ),
            copy_gas(data.len() - 4)
                + copy_gas(output.len())
                + STORAGE_READ_GAS
                    * (NODE_INTERFACE_CONTEXT_STORAGE_READS
                        + NODE_INTERFACE_GAS_ESTIMATE_COMPONENTS_STORAGE_READS)
        );
    }

    #[test]
    fn gas_estimate_l1_component_charges_five_storage_reads() {
        let call = INodeInterfaceVirtual::gasEstimateL1ComponentCall {
            to: Address::with_last_byte(0xaa),
            contractCreation: false,
            data: Bytes::from_static(&[0x95, 0xd8, 0x9b, 0x41]),
        };
        let data = call.abi_encode();
        let output = (1u64, U256::from(2), U256::from(3)).abi_encode();

        assert_eq!(
            ArbitrumApiImpl::<()>::virtual_call_gas(
                &data,
                &output,
                NODE_INTERFACE_GAS_ESTIMATE_L1_COMPONENT_STORAGE_READS,
            ),
            copy_gas(data.len() - 4) + copy_gas(output.len()) + STORAGE_READ_GAS * 5
        );
    }

    #[test]
    fn virtual_success_rejects_insufficient_gas_limit() {
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                gas_limit: 10,
                ..Default::default()
            },
            ArbitrumTxContext::default(),
        );

        let err = ArbitrumApiImpl::<()>::virtual_success::<EmptyDB>(&tx, Bytes::new(), 11)
            .expect_err("insufficient gas should fail");

        assert!(matches!(
            err,
            EVMError::Transaction(InvalidTransaction::CallGasCostMoreThanGasLimit {
                initial_gas: 11,
                gas_limit: 10,
            })
        ));
    }
}
