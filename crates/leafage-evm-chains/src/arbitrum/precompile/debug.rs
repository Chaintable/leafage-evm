use super::abi::IArbDebug;
use super::state::ArbStorage;
use super::util::{copy_gas, dispatch, empty_revert, finish_call, topic_address, topic_u256};
use super::{ArbPrecompileInput, ArbitrumContext, ARB_DEBUG_ADDRESS};
use alloy::primitives::{keccak256, Address, Bytes, Log, B256, U256};
use alloy::sol_types::SolValue;
use revm::context::{ContextTr, JournalTr};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};
use revm::state::Bytecode;
use revm::Database;

pub(super) struct ArbDebug;

const CUSTOM_REVERT_MESSAGE: &str =
    "This spider family wards off bugs: /\\oo/\\ //\\(oo)//\\ /\\oo/\\";
const ARBOS_VERSION_SOLIDITY_REVERTS: u64 = 11;
const ARBOS_VERSION_WRITE_PROTECTION: u64 = 11;
const ARBOS_VERSION_STYLUS: u64 = 30;
const LOG_GAS: u64 = 375;
const LOG_TOPIC_GAS: u64 = 375;
const LOG_DATA_GAS: u64 = 8;

impl ArbDebug {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        if !input.allow_debug_precompiles {
            return empty_revert(gas_limit, gas_limit);
        }

        let caller = input.caller;
        let paid = input.value;
        let is_static = input.is_static;
        let current_arbos_version = input.current_arbos_version;
        let context = input.context;
        dispatch::<IArbDebug::IArbDebugCalls>(input.data, gas_limit, |call, initial_gas| {
            let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
            match call {
                IArbDebug::IArbDebugCalls::becomeChainOwner(_) => {
                    if is_static {
                        return empty_revert(gas_limit, gas_limit);
                    }

                    let owners_key = storage.chain_owner_key();
                    storage.address_set_add(&owners_key, caller)?;
                    finish_call::<IArbDebug::becomeChainOwnerCall>(
                        gas_limit,
                        storage.gas_used,
                        ().into(),
                    )
                }
                IArbDebug::IArbDebugCalls::overwriteContractCode(call) => {
                    if is_static {
                        return empty_revert(gas_limit, gas_limit);
                    }

                    Self::overwrite_contract_code(
                        &mut storage,
                        gas_limit,
                        call.target,
                        call.newCode,
                    )
                }
                IArbDebug::IArbDebugCalls::events(call) => {
                    if is_static {
                        return empty_revert(gas_limit, gas_limit);
                    }

                    Self::emit_events(&mut storage, caller, call.flag, call.value)?;
                    finish_call::<IArbDebug::eventsCall>(
                        gas_limit,
                        storage.gas_used,
                        (caller, paid).into(),
                    )
                }
                IArbDebug::IArbDebugCalls::eventsView(_) => {
                    if storage.arbos_version()? >= ARBOS_VERSION_WRITE_PROTECTION {
                        return empty_revert(gas_limit, storage.gas_used);
                    }

                    Self::emit_events(&mut storage, caller, true, B256::ZERO)?;
                    finish_call::<IArbDebug::eventsViewCall>(gas_limit, storage.gas_used, ().into())
                }
                IArbDebug::IArbDebugCalls::customRevert(call) => {
                    Self::custom_revert(gas_limit, storage.gas_used, call.number)
                }
                IArbDebug::IArbDebugCalls::panic(_) => {
                    if storage.arbos_version()? < ARBOS_VERSION_STYLUS {
                        return empty_revert(gas_limit, gas_limit);
                    }
                    Err(PrecompileError::Fatal("ArbDebug.panic called".to_owned()))
                }
                IArbDebug::IArbDebugCalls::legacyError(_) => {
                    if current_arbos_version < ARBOS_VERSION_SOLIDITY_REVERTS {
                        empty_revert(gas_limit, gas_limit)
                    } else {
                        empty_revert(gas_limit, storage.gas_used)
                    }
                }
            }
        })
    }

    fn overwrite_contract_code<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        gas_limit: u64,
        target: Address,
        new_code: Bytes,
    ) -> PrecompileResult {
        let old_code = {
            let loaded = storage
                .context
                .journal_mut()
                .load_account_with_code(target)
                .map_err(|e| PrecompileError::other(format!("{e:?}")))?;
            loaded
                .data
                .info
                .code
                .as_ref()
                .map(|code| code.original_bytes())
                .unwrap_or_default()
        };

        storage
            .context
            .journal_mut()
            .set_code(target, Bytecode::new_legacy(new_code));
        finish_call::<IArbDebug::overwriteContractCodeCall>(gas_limit, storage.gas_used, old_code)
    }

    fn emit_events<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
        flag: bool,
        value: B256,
    ) -> Result<(), PrecompileError> {
        storage.burn(Self::log_gas(2, 32))?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_DEBUG_ADDRESS,
            vec![keccak256("Basic(bool,bytes32)"), value],
            Bytes::from((!flag,).abi_encode()),
        ));

        storage.burn(Self::log_gas(4, 64))?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_DEBUG_ADDRESS,
            vec![
                keccak256("Mixed(bool,bool,bytes32,address,address)"),
                topic_u256(U256::from(flag as u8)),
                value,
                topic_address(caller),
            ],
            Bytes::from((!flag, ARB_DEBUG_ADDRESS).abi_encode()),
        ));
        Ok(())
    }

    const fn log_gas(topics: u64, data_len: u64) -> u64 {
        LOG_GAS + LOG_TOPIC_GAS * topics + LOG_DATA_GAS * data_len
    }

    fn custom_revert(gas_limit: u64, gas_used: u64, number: u64) -> PrecompileResult {
        if gas_used > gas_limit {
            return Err(PrecompileError::OutOfGas);
        }

        let mut out = Vec::from(&keccak256("Custom(uint64,string,bool)").as_slice()[..4]);
        out.extend((number, CUSTOM_REVERT_MESSAGE.to_owned(), true).abi_encode());
        let gas_used = gas_used.saturating_add(copy_gas(out.len()));
        if gas_used > gas_limit {
            return empty_revert(gas_limit, gas_limit);
        }
        Ok(PrecompileOutput::new_reverted(gas_used, out.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::BASE_PRECOMPILE_GAS;
    use super::*;
    use crate::arbitrum::arbos_state;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::sol_types::SolCall;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::context::JournalTr;
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::{Context, MainContext};

    fn context() -> ArbitrumContext<CacheDB<EmptyDB>> {
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            storage
                .write(&[], arbos_state::ARBOS_VERSION_OFFSET, U256::from(11))
                .expect("write ArbOS version");
        }
        context
    }

    fn run_events_view(
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
        is_static: bool,
    ) -> PrecompileOutput {
        let data = IArbDebug::eventsViewCall {}.abi_encode();
        ArbDebug::run(ArbPrecompileInput {
            data: &data,
            gas: 1_000_000,
            caller: Address::from([0x11; 20]),
            value: U256::ZERO,
            is_static,
            is_valid_call_context: true,
            current_arbos_version: 11,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: true,
            current_chain_config: None,
            context,
        })
        .expect("eventsView")
    }

    fn run_legacy_error(arbos_version: u64, gas_limit: u64) -> PrecompileOutput {
        let mut context = context();
        let data = IArbDebug::legacyErrorCall {}.abi_encode();
        ArbDebug::run(ArbPrecompileInput {
            data: &data,
            gas: gas_limit,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: arbos_version,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: true,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("legacyError")
    }

    #[test]
    fn events_view_reverts_on_regular_call_after_write_protection_version() {
        let mut context = context();

        let output = run_events_view(&mut context, false);

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert!(context.journal_mut().take_logs().is_empty());
    }

    #[test]
    fn events_view_reverts_on_staticcall_after_write_protection_version() {
        let mut context = context();

        let output = run_events_view(&mut context, true);

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert!(context.journal_mut().take_logs().is_empty());
    }

    #[test]
    fn legacy_error_consumes_all_gas_before_solidity_revert_version() {
        let gas_limit = 1_000_000;

        let output = run_legacy_error(10, gas_limit);

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(output.gas_used, gas_limit);
    }

    #[test]
    fn legacy_error_uses_empty_revert_from_solidity_revert_version() {
        let output = run_legacy_error(11, 1_000_000);

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(output.gas_used, BASE_PRECOMPILE_GAS);
    }
}
