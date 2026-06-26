use super::abi::IArbSys;
use super::state::ArbStorage;
use super::util::{
    alias_l1_address, copy_gas, dispatch, empty_revert, finish_call, inverse_alias_l1_address,
    log_gas,
};
use super::{ArbPrecompileInput, ArbitrumContext, ARB_SYS_ADDRESS};
use crate::arbitrum::context::ArbitrumCallContext;
use alloy::primitives::{keccak256, Address, Bytes, Log, B256, U256};
use alloy::sol_types::{SolError, SolEvent};
use revm::context::{Cfg, ContextTr, Transaction};
use revm::context_interface::Block;
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};
use revm::Database;

pub(super) struct ArbSys;

impl ArbSys {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let caller = input.caller;
        let value = input.value;
        let is_static = input.is_static;
        let current_arbos_version = input.current_arbos_version;
        let current_l1_block_number = input.current_l1_block_number;
        let context = input.context;
        let call_context = context.chain().current_call();
        let tx_origin = context.tx().caller();
        let tx_aliases_caller = context.tx().aliases_caller();
        dispatch::<IArbSys::IArbSysCalls>(data, gas_limit, |call, initial_gas| {
            let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
            match call {
                IArbSys::IArbSysCalls::arbBlockNumber(_) => {
                    let ret = storage.current_l2_block_number();
                    finish_call::<IArbSys::arbBlockNumberCall>(gas_limit, storage.gas_used, ret)
                }
                IArbSys::IArbSysCalls::arbBlockHash(call) => {
                    let requested = call.arbBlockNum;
                    let current = storage.current_l2_block_number();
                    let u64_max = U256::from(u64::MAX);
                    let is_not_u64 = requested > u64_max;
                    let current_is_not_u64 = current > u64_max;
                    let is_out_of_range = current_is_not_u64
                        || requested >= current
                        || requested.saturating_add(U256::from(256)) < current;
                    if is_not_u64 || is_out_of_range {
                        return Self::invalid_block_number_revert(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                            requested,
                            current,
                        );
                    }
                    let block = requested.to::<u64>();
                    let hash = storage
                        .context
                        .db_mut()
                        .block_hash(block)
                        .map_err(|e| PrecompileError::other(format!("{e:?}")))?;
                    finish_call::<IArbSys::arbBlockHashCall>(
                        gas_limit,
                        storage.gas_used,
                        B256::from(hash.0),
                    )
                }
                IArbSys::IArbSysCalls::arbChainID(_) => finish_call::<IArbSys::arbChainIDCall>(
                    gas_limit,
                    storage.gas_used,
                    U256::from(storage.context.cfg().chain_id()),
                ),
                IArbSys::IArbSysCalls::arbOSVersion(_) => {
                    let version = storage.arbos_version()?.saturating_add(55);
                    finish_call::<IArbSys::arbOSVersionCall>(
                        gas_limit,
                        storage.gas_used,
                        U256::from(version),
                    )
                }
                IArbSys::IArbSysCalls::getStorageGasAvailable(_) => {
                    finish_call::<IArbSys::getStorageGasAvailableCall>(
                        gas_limit,
                        storage.gas_used,
                        U256::ZERO,
                    )
                }
                IArbSys::IArbSysCalls::isTopLevelCall(_) => {
                    let top_level = call_context.depth <= 2;
                    finish_call::<IArbSys::isTopLevelCallCall>(
                        gas_limit,
                        storage.gas_used,
                        top_level,
                    )
                }
                IArbSys::IArbSysCalls::mapL1SenderContractAddressToL2Alias(call) => {
                    finish_call::<IArbSys::mapL1SenderContractAddressToL2AliasCall>(
                        gas_limit,
                        storage.gas_used,
                        alias_l1_address(call.sender),
                    )
                }
                IArbSys::IArbSysCalls::wasMyCallersAddressAliased(_) => {
                    let aliased = Self::was_my_callers_address_aliased(
                        current_arbos_version,
                        call_context,
                        tx_origin,
                        tx_aliases_caller,
                    );
                    finish_call::<IArbSys::wasMyCallersAddressAliasedCall>(
                        gas_limit,
                        storage.gas_used,
                        aliased,
                    )
                }
                IArbSys::IArbSysCalls::myCallersAddressWithoutAliasing(_) => {
                    let caller_without_alias = Self::my_callers_address_without_aliasing(
                        current_arbos_version,
                        call_context,
                        tx_origin,
                        tx_aliases_caller,
                    );
                    finish_call::<IArbSys::myCallersAddressWithoutAliasingCall>(
                        gas_limit,
                        storage.gas_used,
                        caller_without_alias,
                    )
                }
                IArbSys::IArbSysCalls::sendMerkleTreeState(_) => {
                    if !caller.is_zero() {
                        return Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        );
                    }
                    let (size, root, partials) = storage.send_merkle_state()?;
                    finish_call::<IArbSys::sendMerkleTreeStateCall>(
                        gas_limit,
                        storage.gas_used,
                        (U256::from(size), root, partials).into(),
                    )
                }
                IArbSys::IArbSysCalls::withdrawEth(call) => {
                    if is_static {
                        return empty_revert(gas_limit, storage.gas_used);
                    }
                    match Self::send_tx_to_l1(
                        &mut storage,
                        caller,
                        value,
                        call.destination,
                        Bytes::new(),
                        current_arbos_version,
                        current_l1_block_number,
                    ) {
                        Ok(outcome) => finish_call::<IArbSys::withdrawEthCall>(
                            gas_limit,
                            storage.gas_used,
                            Self::send_tx_to_l1_return(outcome, current_arbos_version),
                        ),
                        Err(PrecompileError::Other(_)) => Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        ),
                        Err(error) => Err(error),
                    }
                }
                IArbSys::IArbSysCalls::sendTxToL1(call) => {
                    if is_static {
                        return empty_revert(gas_limit, storage.gas_used);
                    }
                    match Self::send_tx_to_l1(
                        &mut storage,
                        caller,
                        value,
                        call.destination,
                        call.data.clone(),
                        current_arbos_version,
                        current_l1_block_number,
                    ) {
                        Ok(outcome) => finish_call::<IArbSys::sendTxToL1Call>(
                            gas_limit,
                            storage.gas_used,
                            Self::send_tx_to_l1_return(outcome, current_arbos_version),
                        ),
                        Err(PrecompileError::Other(_)) => Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        ),
                        Err(error) => Err(error),
                    }
                }
            }
        })
    }

    fn invalid_block_number_revert(
        gas_limit: u64,
        gas_used: u64,
        current_arbos_version: u64,
        requested: U256,
        current: U256,
    ) -> PrecompileResult {
        if current_arbos_version < 11 {
            return Self::non_solidity_error(gas_limit, gas_used, current_arbos_version);
        }

        if gas_used > gas_limit {
            return Err(PrecompileError::OutOfGas);
        }

        let error = IArbSys::InvalidBlockNumber { requested, current };
        let bytes = Bytes::from(error.abi_encode());
        let gas_used = gas_used.saturating_add(copy_gas(bytes.len()));
        if gas_used > gas_limit {
            return empty_revert(gas_limit, gas_limit);
        }
        Ok(PrecompileOutput::new_reverted(gas_used, bytes))
    }

    fn non_solidity_error(
        gas_limit: u64,
        gas_used: u64,
        current_arbos_version: u64,
    ) -> PrecompileResult {
        if current_arbos_version < 11 {
            return empty_revert(gas_limit, gas_limit);
        }
        empty_revert(gas_limit, gas_used)
    }

    fn was_my_callers_address_aliased(
        current_arbos_version: u64,
        call_context: ArbitrumCallContext,
        tx_origin: Address,
        tx_aliases_caller: bool,
    ) -> bool {
        let top_level = if current_arbos_version < 6 {
            call_context.depth == 2
        } else {
            call_context.depth < 2 || call_context.callers_caller == tx_origin
        };
        top_level && tx_aliases_caller
    }

    fn my_callers_address_without_aliasing(
        current_arbos_version: u64,
        call_context: ArbitrumCallContext,
        tx_origin: Address,
        tx_aliases_caller: bool,
    ) -> Address {
        let mut address = if call_context.depth > 1 {
            call_context.callers_caller
        } else {
            Address::ZERO
        };

        if Self::was_my_callers_address_aliased(
            current_arbos_version,
            call_context,
            tx_origin,
            tx_aliases_caller,
        ) {
            address = inverse_alias_l1_address(address);
        }
        address
    }

    fn send_tx_to_l1<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
        value: U256,
        destination: Address,
        calldata_for_l1: Bytes,
        current_arbos_version: u64,
        current_l1_block_number: u64,
    ) -> Result<SendTxToL1Outcome, PrecompileError> {
        if !value.is_zero() && current_arbos_version >= 41 {
            let owners_key = storage.native_token_owner_key();
            if storage.read_u64(&owners_key, 0)? > 0 {
                return Err(PrecompileError::other(
                    "not allowed to send value when native token owners exist",
                ));
            }
        }

        let arb_block_num = storage.current_l2_block_number();
        let eth_block_num = U256::from(current_l1_block_number);
        let timestamp = storage.context.block().timestamp();
        let send_hash = Self::l2_to_l1_hash(
            caller,
            destination,
            arb_block_num,
            eth_block_num,
            timestamp,
            value,
            calldata_for_l1.as_ref(),
        );
        let (size, updates) = storage.send_merkle_append(send_hash)?;
        let leaf = size.saturating_sub(1);

        storage.burn_precompile_balance(ARB_SYS_ADDRESS, value)?;

        for update in updates {
            Self::emit_send_merkle_update(storage, update.level, update.num_leaves, update.hash)?;
        }
        Self::emit_l2_to_l1_tx(
            storage,
            caller,
            destination,
            send_hash,
            leaf,
            arb_block_num,
            eth_block_num,
            timestamp,
            value,
            calldata_for_l1,
        )?;

        Ok(SendTxToL1Outcome { leaf, send_hash })
    }

    fn send_tx_to_l1_return(outcome: SendTxToL1Outcome, current_arbos_version: u64) -> U256 {
        if current_arbos_version >= 4 {
            U256::from(outcome.leaf)
        } else {
            U256::from_be_slice(outcome.send_hash.as_slice())
        }
    }

    fn l2_to_l1_hash(
        caller: Address,
        destination: Address,
        arb_block_num: U256,
        eth_block_num: U256,
        timestamp: U256,
        value: U256,
        calldata_for_l1: &[u8],
    ) -> B256 {
        let mut data = Vec::with_capacity(20 + 20 + 32 * 4 + calldata_for_l1.len());
        data.extend_from_slice(caller.as_slice());
        data.extend_from_slice(destination.as_slice());
        data.extend_from_slice(&arb_block_num.to_be_bytes::<32>());
        data.extend_from_slice(&eth_block_num.to_be_bytes::<32>());
        data.extend_from_slice(&timestamp.to_be_bytes::<32>());
        data.extend_from_slice(&value.to_be_bytes::<32>());
        data.extend_from_slice(calldata_for_l1);
        keccak256(data)
    }

    fn emit_send_merkle_update<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        level: u64,
        leaf: u64,
        hash: B256,
    ) -> Result<(), PrecompileError> {
        let position = (U256::from(level) << 192) | U256::from(leaf);
        let event = IArbSys::SendMerkleUpdate {
            reserved: U256::ZERO,
            hash,
            position,
        };
        storage.burn(log_gas(3, 0))?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_SYS_ADDRESS,
            event.encode_topics().into_iter().map(Into::into).collect(),
            event.encode_data().into(),
        ));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_l2_to_l1_tx<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
        destination: Address,
        send_hash: B256,
        leaf: u64,
        arb_block_num: U256,
        eth_block_num: U256,
        timestamp: U256,
        value: U256,
        calldata_for_l1: Bytes,
    ) -> Result<(), PrecompileError> {
        let event = IArbSys::L2ToL1Tx {
            caller,
            destination,
            hash: U256::from_be_slice(send_hash.as_slice()),
            position: U256::from(leaf),
            arbBlockNum: arb_block_num,
            ethBlockNum: eth_block_num,
            timestamp,
            callvalue: value,
            data: calldata_for_l1,
        };
        let data = Bytes::from(event.encode_data());
        storage.burn(log_gas(3, data.len()))?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_SYS_ADDRESS,
            event.encode_topics().into_iter().map(Into::into).collect(),
            data,
        ));
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct SendTxToL1Outcome {
    leaf: u64,
    send_hash: B256,
}

#[cfg(test)]
mod tests {
    use super::super::util::{topic_address, topic_u256};
    use super::*;
    use crate::arbitrum::arbos_state;
    use crate::arbitrum::context::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::sol_types::{SolCall, SolError, SolEvent};
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::context::{JournalTr, TxEnv};
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::precompile::PrecompileOutput;
    use revm::state::AccountInfo;
    use revm::{Context, MainContext};

    fn context_with_tx(tx: ArbitrumTxEnv) -> ArbitrumContext<CacheDB<EmptyDB>> {
        Context::mainnet()
            .with_tx(tx)
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(ArbitrumExecutionContext::default())
    }

    fn run_arb_sys_view(
        data: &[u8],
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
        current_arbos_version: u64,
    ) -> PrecompileOutput {
        ArbSys::run(ArbPrecompileInput {
            data,
            gas: 10_000_000,
            caller: Address::with_last_byte(0xaa),
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context,
        })
        .expect("ArbSys view call")
    }

    #[test]
    fn arb_block_hash_invalid_number_uses_custom_error_from_arbos_11() {
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv {
                number: U256::from(1),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");

        let requested = U256::from(1_000_000_000u64);
        let input = IArbSys::arbBlockHashCall {
            arbBlockNum: requested,
        }
        .abi_encode();
        let output = ArbSys::run(ArbPrecompileInput {
            data: &input,
            gas: 10_000_000,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 11,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("arbBlockHash");

        assert!(output.reverted);
        let error = IArbSys::InvalidBlockNumber::abi_decode(output.bytes.as_ref())
            .expect("decode InvalidBlockNumber");
        assert_eq!(error.requested, requested);
        assert_eq!(error.current, U256::from(1));

        let legacy_gas_limit = 10_000_000;
        let legacy = ArbSys::run(ArbPrecompileInput {
            data: &input,
            gas: legacy_gas_limit,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 10,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("legacy arbBlockHash");
        assert!(legacy.reverted);
        assert!(legacy.bytes.is_empty());
        assert_eq!(legacy.gas_used, legacy_gas_limit);

        let oversized = IArbSys::arbBlockHashCall {
            arbBlockNum: U256::from(u64::MAX) + U256::from(1),
        }
        .abi_encode();
        let oversized_legacy_gas_limit = 10_000_000;
        let oversized_legacy = ArbSys::run(ArbPrecompileInput {
            data: &oversized,
            gas: oversized_legacy_gas_limit,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 10,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("oversized legacy arbBlockHash");
        assert!(oversized_legacy.reverted);
        assert!(oversized_legacy.bytes.is_empty());
        assert_eq!(oversized_legacy.gas_used, oversized_legacy_gas_limit);

        let oversized_current = U256::from(u64::MAX) + U256::from(2);
        let mut oversized_current_context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv {
                number: oversized_current,
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(ArbitrumExecutionContext::default());
        oversized_current_context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");

        let requested_max = U256::from(u64::MAX);
        let oversized_current_input = IArbSys::arbBlockHashCall {
            arbBlockNum: requested_max,
        }
        .abi_encode();
        let oversized_current_output = ArbSys::run(ArbPrecompileInput {
            data: &oversized_current_input,
            gas: 10_000_000,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 11,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut oversized_current_context,
        })
        .expect("oversized current arbBlockHash");
        assert!(oversized_current_output.reverted);
        let error =
            IArbSys::InvalidBlockNumber::abi_decode(oversized_current_output.bytes.as_ref())
                .expect("decode oversized current InvalidBlockNumber");
        assert_eq!(error.requested, requested_max);
        assert_eq!(error.current, oversized_current);
    }

    #[test]
    fn send_merkle_tree_state_nonzero_caller_uses_legacy_error_semantics() {
        let input = IArbSys::sendMerkleTreeStateCall {}.abi_encode();
        let gas_limit = 10_000_000;
        let caller = Address::with_last_byte(0xaa);
        let mut context = context_with_tx(ArbitrumTxEnv::default());

        let modern = ArbSys::run(ArbPrecompileInput {
            data: &input,
            gas: gas_limit,
            caller,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 11,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("sendMerkleTreeState");
        assert!(modern.reverted);
        assert!(modern.bytes.is_empty());
        assert!(modern.gas_used < gas_limit);

        let legacy = ArbSys::run(ArbPrecompileInput {
            data: &input,
            gas: gas_limit,
            caller,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 10,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("legacy sendMerkleTreeState");
        assert!(legacy.reverted);
        assert!(legacy.bytes.is_empty());
        assert_eq!(legacy.gas_used, gas_limit);
    }

    #[test]
    fn direct_call_reports_no_caller_alias_and_zero_address() {
        let mut context = context_with_tx(ArbitrumTxEnv::default());

        let was_input = IArbSys::wasMyCallersAddressAliasedCall {}.abi_encode();
        let was_output = run_arb_sys_view(&was_input, &mut context, 60);
        assert_eq!(U256::from_be_slice(was_output.bytes.as_ref()), U256::ZERO);

        let caller_input = IArbSys::myCallersAddressWithoutAliasingCall {}.abi_encode();
        let caller_output = run_arb_sys_view(&caller_input, &mut context, 60);
        assert_eq!(
            Address::from_slice(&caller_output.bytes.as_ref()[12..]),
            Address::ZERO
        );
    }

    #[test]
    fn arb_block_number_uses_l2_execution_context_when_evm_block_is_l1() {
        let l1_block = U256::from(99_999u64);
        let l2_block = U256::from(123_456u64);
        let mut chain_context = ArbitrumExecutionContext::default();
        chain_context.set_current_l2_context(l2_block, 10_000_000);
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv {
                number: l1_block,
                basefee: 0,
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(chain_context);

        let input = IArbSys::arbBlockNumberCall {}.abi_encode();
        let output = run_arb_sys_view(&input, &mut context, 60);
        let ret = IArbSys::arbBlockNumberCall::abi_decode_returns(output.bytes.as_ref())
            .expect("decode arbBlockNumber return");

        assert_eq!(ret, l2_block);
    }

    #[test]
    fn top_level_retryable_call_reports_unaliased_parent_caller() {
        let l1_origin = Address::from([0x22; 20]);
        let aliased_origin = alias_l1_address(l1_origin);
        let tx = ArbitrumTxEnv::retryable_redeem(
            TxEnv {
                caller: aliased_origin,
                ..Default::default()
            },
            None,
            Address::ZERO,
            Default::default(),
        );
        let mut context = context_with_tx(tx);
        context.chain.set_current_call(2, aliased_origin);

        let was_input = IArbSys::wasMyCallersAddressAliasedCall {}.abi_encode();
        let was_output = run_arb_sys_view(&was_input, &mut context, 60);
        assert_eq!(
            U256::from_be_slice(was_output.bytes.as_ref()),
            U256::from(1)
        );

        let caller_input = IArbSys::myCallersAddressWithoutAliasingCall {}.abi_encode();
        let caller_output = run_arb_sys_view(&caller_input, &mut context, 60);
        assert_eq!(
            Address::from_slice(&caller_output.bytes.as_ref()[12..]),
            l1_origin
        );
    }

    #[test]
    fn nested_retryable_call_reports_parent_contract_without_unaliasing() {
        let aliased_origin = alias_l1_address(Address::from([0x33; 20]));
        let parent_contract = Address::from([0x44; 20]);
        let tx = ArbitrumTxEnv::retryable_redeem(
            TxEnv {
                caller: aliased_origin,
                ..Default::default()
            },
            None,
            Address::ZERO,
            Default::default(),
        );
        let mut context = context_with_tx(tx);
        context.chain.set_current_call(3, parent_contract);

        let was_input = IArbSys::wasMyCallersAddressAliasedCall {}.abi_encode();
        let was_output = run_arb_sys_view(&was_input, &mut context, 60);
        assert_eq!(U256::from_be_slice(was_output.bytes.as_ref()), U256::ZERO);

        let caller_input = IArbSys::myCallersAddressWithoutAliasingCall {}.abi_encode();
        let caller_output = run_arb_sys_view(&caller_input, &mut context, 60);
        assert_eq!(
            Address::from_slice(&caller_output.bytes.as_ref()[12..]),
            parent_contract
        );
    }

    #[test]
    fn send_merkle_update_event_charges_generated_log_gas_before_log() {
        let mut context = context_with_tx(ArbitrumTxEnv::default());
        let hash = B256::from([0x33; 32]);
        let cost = log_gas(3, 0);

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, cost - 1, 0);
            let error = ArbSys::emit_send_merkle_update(&mut storage, 1, 2, hash)
                .expect_err("event should run out of gas");
            assert!(error.is_oog());
        }
        assert!(context.journal_mut().take_logs().is_empty());

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, cost, 0);
            ArbSys::emit_send_merkle_update(&mut storage, 1, 2, hash)
                .expect("emit SendMerkleUpdate");
            assert_eq!(storage.gas_used, cost);
        }
        let logs = context.journal_mut().take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].data.topics()[0],
            keccak256("SendMerkleUpdate(uint256,bytes32,uint256)")
        );
        assert!(logs[0].data.data.is_empty());
    }

    #[test]
    fn l2_to_l1_tx_event_charges_encoded_data_log_gas_before_log() {
        let caller = Address::from([0x61; 20]);
        let destination = Address::from([0x62; 20]);
        let send_hash = B256::from([0x63; 32]);
        let empty_cost = log_gas(3, 224);
        let mut context = context_with_tx(ArbitrumTxEnv::default());

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, empty_cost - 1, 0);
            let error = ArbSys::emit_l2_to_l1_tx(
                &mut storage,
                caller,
                destination,
                send_hash,
                0,
                U256::from(1),
                U256::from(2),
                U256::from(3),
                U256::from(4),
                Bytes::new(),
            )
            .expect_err("event should run out of gas");
            assert!(error.is_oog());
        }
        assert!(context.journal_mut().take_logs().is_empty());

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, empty_cost, 0);
            ArbSys::emit_l2_to_l1_tx(
                &mut storage,
                caller,
                destination,
                send_hash,
                0,
                U256::from(1),
                U256::from(2),
                U256::from(3),
                U256::from(4),
                Bytes::new(),
            )
            .expect("emit L2ToL1Tx");
            assert_eq!(storage.gas_used, empty_cost);
        }
        let logs = context.journal_mut().take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].data.data.len(), 224);

        let mut payload_context = context_with_tx(ArbitrumTxEnv::default());
        let payload_cost = log_gas(3, 256);
        {
            let mut storage =
                ArbStorage::new_with_initial_gas(&mut payload_context, payload_cost, 0);
            ArbSys::emit_l2_to_l1_tx(
                &mut storage,
                caller,
                destination,
                send_hash,
                0,
                U256::from(1),
                U256::from(2),
                U256::from(3),
                U256::from(4),
                Bytes::from_static(b"payload"),
            )
            .expect("emit L2ToL1Tx with payload");
            assert_eq!(storage.gas_used, payload_cost);
        }
        let logs = payload_context.journal_mut().take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].data.data.len(), 256);
    }

    #[test]
    fn send_tx_to_l1_emits_l1_block_number_and_hash() {
        let caller = Address::from([1; 20]);
        let destination = Address::from([2; 20]);
        let arb_block_num = U256::from(10);
        let eth_block_num = U256::from(42);
        let timestamp = U256::from(20);
        let value = U256::from(30);
        let calldata = Bytes::from_static(b"payload");
        let expected_send_hash = ArbSys::l2_to_l1_hash(
            caller,
            destination,
            arb_block_num,
            eth_block_num,
            timestamp,
            value,
            calldata.as_ref(),
        );

        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            ARB_SYS_ADDRESS,
            AccountInfo {
                balance: value,
                ..Default::default()
            },
        );

        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv {
                number: arb_block_num,
                timestamp,
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(db)
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");

        let warmup_input = IArbSys::sendTxToL1Call {
            destination: Address::with_last_byte(0xee),
            data: Bytes::new(),
        }
        .abi_encode();
        ArbSys::run(ArbPrecompileInput {
            data: &warmup_input,
            gas: 10_000_000,
            caller,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 4,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: eth_block_num.to(),
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("warmup sendTxToL1");
        let _ = context.journal_mut().take_logs();

        let input = IArbSys::sendTxToL1Call {
            destination,
            data: calldata.clone(),
        }
        .abi_encode();

        let output = ArbSys::run(ArbPrecompileInput {
            data: &input,
            gas: 10_000_000,
            caller,
            value,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 4,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: eth_block_num.to(),
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("sendTxToL1");
        assert_eq!(output.bytes.len(), 32);
        assert_eq!(U256::from_be_slice(output.bytes.as_ref()), U256::from(1));

        let logs = context.journal_mut().take_logs();
        let log = logs
            .iter()
            .find(|log| {
                log.address == ARB_SYS_ADDRESS
                    && log.data.topics().first()
                        == Some(&keccak256(
                            "L2ToL1Tx(address,address,uint256,uint256,uint256,uint256,uint256,uint256,bytes)",
                        ))
            })
            .expect("L2ToL1Tx log");

        let topics = log.data.topics();
        assert_eq!(topics[1], topic_address(destination));
        assert_eq!(topics[2], expected_send_hash);
        assert_eq!(topics[3], topic_u256(U256::from(1)));

        let (
            event_caller,
            event_arb_block_num,
            event_eth_block_num,
            event_timestamp,
            event_value,
            event_calldata,
        ) = IArbSys::L2ToL1Tx::abi_decode_data(log.data.data.as_ref())
            .expect("decode L2ToL1Tx data");
        assert_eq!(event_caller, caller);
        assert_eq!(event_arb_block_num, arb_block_num);
        assert_eq!(event_eth_block_num, eth_block_num);
        assert_eq!(event_timestamp, timestamp);
        assert_eq!(event_value, value);
        assert_eq!(event_calldata, calldata);
    }

    #[test]
    fn withdraw_eth_emits_l1_block_number_and_hash() {
        let caller = Address::from([3; 20]);
        let destination = Address::from([4; 20]);
        let arb_block_num = U256::from(11);
        let eth_block_num = U256::from(43);
        let timestamp = U256::from(21);
        let value = U256::from(31);
        let calldata = Bytes::new();
        let expected_send_hash = ArbSys::l2_to_l1_hash(
            caller,
            destination,
            arb_block_num,
            eth_block_num,
            timestamp,
            value,
            calldata.as_ref(),
        );

        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            ARB_SYS_ADDRESS,
            AccountInfo {
                balance: value,
                ..Default::default()
            },
        );

        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv {
                number: arb_block_num,
                timestamp,
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(db)
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");

        let input = IArbSys::withdrawEthCall { destination }.abi_encode();

        let output = ArbSys::run(ArbPrecompileInput {
            data: &input,
            gas: 10_000_000,
            caller,
            value,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 3,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: eth_block_num.to(),
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("withdrawEth");
        assert_eq!(output.bytes.len(), 32);
        assert_eq!(
            U256::from_be_slice(output.bytes.as_ref()),
            U256::from_be_slice(expected_send_hash.as_slice())
        );

        let logs = context.journal_mut().take_logs();
        let log = logs
            .iter()
            .find(|log| {
                log.address == ARB_SYS_ADDRESS
                    && log.data.topics().first()
                        == Some(&keccak256(
                            "L2ToL1Tx(address,address,uint256,uint256,uint256,uint256,uint256,uint256,bytes)",
                        ))
            })
            .expect("L2ToL1Tx log");

        let topics = log.data.topics();
        assert_eq!(topics[1], topic_address(destination));
        assert_eq!(topics[2], expected_send_hash);
        assert_eq!(topics[3], topic_u256(U256::ZERO));

        let (
            event_caller,
            event_arb_block_num,
            event_eth_block_num,
            event_timestamp,
            event_value,
            event_calldata,
        ) = IArbSys::L2ToL1Tx::abi_decode_data(log.data.data.as_ref())
            .expect("decode L2ToL1Tx data");
        assert_eq!(event_caller, caller);
        assert_eq!(event_arb_block_num, arb_block_num);
        assert_eq!(event_eth_block_num, eth_block_num);
        assert_eq!(event_timestamp, timestamp);
        assert_eq!(event_value, value);
        assert_eq!(event_calldata, calldata);
    }

    #[test]
    fn send_tx_to_l1_value_gate_uses_current_arbos_version() {
        let caller = Address::from([5; 20]);
        let destination = Address::from([6; 20]);
        let value = U256::from(1);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            ARB_SYS_ADDRESS,
            AccountInfo {
                balance: value,
                ..Default::default()
            },
        );

        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(db)
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, 10_000_000, 0);
            let owners_key = storage.native_token_owner_key();
            storage
                .address_set_add(&owners_key, Address::from([7; 20]))
                .expect("add native token owner");
        }

        let input = IArbSys::sendTxToL1Call {
            destination,
            data: Bytes::new(),
        }
        .abi_encode();

        let rejected = ArbSys::run(ArbPrecompileInput {
            data: &input,
            gas: 10_000_000,
            caller,
            value,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 41,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("sendTxToL1 rejects value");
        assert!(rejected.reverted);
        assert!(context.journal_mut().take_logs().is_empty());
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, 10_000_000, 0);
            let (size, _, _) = storage.send_merkle_state().expect("send merkle state");
            assert_eq!(size, 0);
        }

        let accepted = ArbSys::run(ArbPrecompileInput {
            data: &input,
            gas: 10_000_000,
            caller,
            value,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 40,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("sendTxToL1 before ArbOS 41");
        assert!(!accepted.reverted);
    }
}
