use super::abi::IArbFilteredTransactionsManager;
use super::state::ArbStorage;
use super::util::{dispatch, empty_revert, finish_call, log_gas};
use super::{ArbPrecompileInput, ArbitrumContext, ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS};
use crate::arbitrum::arbos_state;
use alloy::primitives::{keccak256, Address, Bytes, Log, B256};
use revm::context::{ContextTr, JournalTr};
use revm::precompile::{PrecompileError, PrecompileResult};
use revm::Database;

pub(super) struct ArbFilteredTransactionsManager;

impl ArbFilteredTransactionsManager {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let caller = input.caller;
        let is_static = input.is_static;
        let context = input.context;

        dispatch::<IArbFilteredTransactionsManager::IArbFilteredTransactionsManagerCalls>(
            data,
            gas_limit,
            |call, initial_gas| {
                let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
                match call {
                IArbFilteredTransactionsManager::IArbFilteredTransactionsManagerCalls::isTransactionFiltered(call) => {
                    let ret = storage.is_filtered_transaction(call.txHash)?;
                    finish_call::<IArbFilteredTransactionsManager::isTransactionFilteredCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbFilteredTransactionsManager::IArbFilteredTransactionsManagerCalls::addFilteredTransaction(call) => {
                    Self::mutate(&mut storage, gas_limit, caller, is_static, call.txHash, true)
                }
                IArbFilteredTransactionsManager::IArbFilteredTransactionsManagerCalls::deleteFilteredTransaction(call) => {
                    Self::mutate(&mut storage, gas_limit, caller, is_static, call.txHash, false)
                }
            }
            },
        )
    }

    fn mutate<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        gas_limit: u64,
        caller: Address,
        is_static: bool,
        tx_hash: B256,
        add: bool,
    ) -> PrecompileResult {
        if is_static {
            return empty_revert(gas_limit, storage.gas_used);
        }
        if !Self::has_access(storage, caller)? {
            return Err(PrecompileError::OutOfGas);
        }

        if add {
            storage.add_filtered_transaction(tx_hash)?;
            Self::emit(storage, "FilteredTransactionAdded(bytes32)", tx_hash)?;
            finish_call::<IArbFilteredTransactionsManager::addFilteredTransactionCall>(
                gas_limit,
                storage.gas_used,
                ().into(),
            )
        } else {
            storage.delete_filtered_transaction(tx_hash)?;
            Self::emit(storage, "FilteredTransactionDeleted(bytes32)", tx_hash)?;
            finish_call::<IArbFilteredTransactionsManager::deleteFilteredTransactionCall>(
                gas_limit,
                storage.gas_used,
                ().into(),
            )
        }
    }

    pub(super) fn wrapper_access<DB: Database>(
        context: &mut ArbitrumContext<DB>,
        gas_limit: u64,
        caller: Address,
    ) -> Result<(bool, u64), PrecompileError> {
        let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, 0);
        if storage.root(arbos_state::ARBOS_VERSION_OFFSET)?.is_zero() {
            return Err(PrecompileError::other("ArbOS uninitialized"));
        }
        let caller_is_filterer = Self::has_access(&mut storage, caller)?;
        Ok((caller_is_filterer, storage.gas_used))
    }

    fn has_access<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
    ) -> Result<bool, PrecompileError> {
        let filterers_key = storage.transaction_filterer_key();
        storage.address_set_contains(&filterers_key, caller)
    }

    pub(super) fn finish_free_access_call(
        gas_limit: u64,
        result: PrecompileResult,
        caller_is_filterer: bool,
        wrapper_gas_used: u64,
    ) -> PrecompileResult {
        let final_gas = if caller_is_filterer {
            0
        } else {
            wrapper_gas_used
        };
        match result {
            Ok(mut output) => {
                output.gas_used = final_gas;
                Ok(output)
            }
            Err(PrecompileError::Fatal(err)) => Err(PrecompileError::Fatal(err)),
            Err(_) => empty_revert(gas_limit, final_gas),
        }
    }

    fn emit<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        event: &'static str,
        tx_hash: B256,
    ) -> Result<(), PrecompileError> {
        storage.burn(log_gas(1, 0))?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS,
            vec![keccak256(event), tx_hash],
            Bytes::new(),
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ArbitrumPrecompileEnv, ArbitrumPrecompiles, STORAGE_READ_GAS};
    use super::*;
    use crate::arbitrum::arbos_state;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::primitives::U256;
    use alloy::sol_types::SolCall;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::context::JournalTr;
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::handler::PrecompileProvider;
    use revm::interpreter::{
        CallInput, CallInputs, CallScheme, CallValue, InstructionResult, InterpreterResult,
    };
    use revm::{Context, MainContext};

    const WRAPPER_ACCESS_GAS: u64 = STORAGE_READ_GAS * 2;

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
        context
            .journal_mut()
            .load_account(arbos_state::FILTERED_TRANSACTIONS_STATE_ADDRESS)
            .expect("load filtered transactions state account");
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            storage
                .write(&[], arbos_state::ARBOS_VERSION_OFFSET, U256::from(60))
                .expect("initialize ArbOS version");
        }
        context
    }

    fn add_filterer<DB: Database>(context: &mut ArbitrumContext<DB>, filterer: Address) {
        let mut storage = ArbStorage::new_with_initial_gas(context, u64::MAX, 0);
        let filterers_key = storage.transaction_filterer_key();
        storage
            .address_set_add(&filterers_key, filterer)
            .expect("add transaction filterer");
    }

    fn set_filtered<DB: Database>(context: &mut ArbitrumContext<DB>, tx_hash: B256) {
        let mut storage = ArbStorage::new_with_initial_gas(context, u64::MAX, 0);
        storage
            .add_filtered_transaction(tx_hash)
            .expect("set filtered transaction");
    }

    fn is_filtered<DB: Database>(context: &mut ArbitrumContext<DB>, tx_hash: B256) -> bool {
        let mut storage = ArbStorage::new_with_initial_gas(context, u64::MAX, 0);
        storage
            .is_filtered_transaction(tx_hash)
            .expect("read filtered transaction")
    }

    fn provider_call(
        data: &[u8],
        caller: Address,
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
    ) -> InterpreterResult {
        provider_call_with(
            data,
            caller,
            context,
            false,
            U256::ZERO,
            CallScheme::Call,
            ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS,
        )
    }

    fn provider_call_with(
        data: &[u8],
        caller: Address,
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
        is_static: bool,
        value: U256,
        scheme: CallScheme,
        target_address: Address,
    ) -> InterpreterResult {
        let mut precompiles = ArbitrumPrecompiles::new_with_env(
            ArbitrumHardfork::Prague,
            ArbitrumPrecompileEnv {
                current_arbos_version: 60,
                ..Default::default()
            },
        );
        let inputs = CallInputs {
            input: CallInput::Bytes(Bytes::copy_from_slice(data)),
            return_memory_offset: 0..0,
            gas_limit: 1_000_000,
            bytecode_address: ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS,
            known_bytecode: None,
            target_address,
            caller,
            value: CallValue::Transfer(value),
            scheme,
            is_static,
        };

        PrecompileProvider::<ArbitrumContext<CacheDB<EmptyDB>>>::run(
            &mut precompiles,
            context,
            &inputs,
        )
        .expect("provider run should not fail")
        .expect("filtered transactions precompile should be handled")
    }

    #[test]
    fn filterer_mutation_is_free_and_still_emits_event() {
        let filterer = Address::from([0x11; 20]);
        let tx_hash = B256::from([0x22; 32]);
        let mut context = context();
        add_filterer(&mut context, filterer);
        let input = IArbFilteredTransactionsManager::addFilteredTransactionCall { txHash: tx_hash }
            .abi_encode();

        let result = provider_call(&input, filterer, &mut context);

        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(result.gas.spent(), 0);
        assert!(result.output.is_empty());
        assert!(is_filtered(&mut context, tx_hash));

        let logs = context.journal_mut().take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].data.topics()[0],
            keccak256("FilteredTransactionAdded(bytes32)")
        );
        assert_eq!(logs[0].data.topics()[1], tx_hash);
    }

    #[test]
    fn filterer_delete_is_free_and_emits_event() {
        let filterer = Address::from([0x77; 20]);
        let tx_hash = B256::from([0x88; 32]);
        let mut context = context();
        add_filterer(&mut context, filterer);
        set_filtered(&mut context, tx_hash);
        let input =
            IArbFilteredTransactionsManager::deleteFilteredTransactionCall { txHash: tx_hash }
                .abi_encode();

        let result = provider_call(&input, filterer, &mut context);

        assert_eq!(result.result, InstructionResult::Return);
        assert_eq!(result.gas.spent(), 0);
        assert!(!is_filtered(&mut context, tx_hash));

        let logs = context.journal_mut().take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].data.topics()[0],
            keccak256("FilteredTransactionDeleted(bytes32)")
        );
        assert_eq!(logs[0].data.topics()[1], tx_hash);
    }

    #[test]
    fn non_filterer_view_charges_only_wrapper_access_check() {
        let caller = Address::from([0x33; 20]);
        let tx_hash = B256::from([0x44; 32]);
        let mut context = context();
        set_filtered(&mut context, tx_hash);
        let input = IArbFilteredTransactionsManager::isTransactionFilteredCall { txHash: tx_hash }
            .abi_encode();

        let result = provider_call(&input, caller, &mut context);
        let ret = IArbFilteredTransactionsManager::isTransactionFilteredCall::abi_decode_returns(
            result.output.as_ref(),
        )
        .expect("decode isTransactionFiltered return");

        assert_eq!(result.result, InstructionResult::Return);
        assert!(ret);
        assert_eq!(result.gas.spent(), WRAPPER_ACCESS_GAS);
    }

    #[test]
    fn non_filterer_mutation_charges_wrapper_check_without_state_change_or_log() {
        let caller = Address::from([0x55; 20]);
        let tx_hash = B256::from([0x66; 32]);
        let mut context = context();
        let input = IArbFilteredTransactionsManager::addFilteredTransactionCall { txHash: tx_hash }
            .abi_encode();

        let result = provider_call(&input, caller, &mut context);

        assert_eq!(result.result, InstructionResult::Revert);
        assert_eq!(result.gas.spent(), WRAPPER_ACCESS_GAS);
        assert!(!is_filtered(&mut context, tx_hash));
        assert!(context.journal_mut().take_logs().is_empty());
    }

    #[test]
    fn filterer_provider_reverts_are_still_free() {
        let filterer = Address::from([0x99; 20]);
        let tx_hash = B256::from([0xaa; 32]);
        let mut context = context();
        add_filterer(&mut context, filterer);
        let input = IArbFilteredTransactionsManager::addFilteredTransactionCall { txHash: tx_hash }
            .abi_encode();

        let static_result = provider_call_with(
            &input,
            filterer,
            &mut context,
            true,
            U256::ZERO,
            CallScheme::Call,
            ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS,
        );
        assert_eq!(static_result.result, InstructionResult::Revert);
        assert_eq!(static_result.gas.spent(), 0);
        assert!(!is_filtered(&mut context, tx_hash));

        let unknown_selector = [0xff, 0xff, 0xff, 0xff];
        let unknown_result = provider_call(&unknown_selector, filterer, &mut context);
        assert_eq!(unknown_result.result, InstructionResult::Revert);
        assert_eq!(unknown_result.gas.spent(), 0);
        assert!(context.journal_mut().take_logs().is_empty());
    }

    #[test]
    fn non_filterer_provider_reverts_charge_wrapper_access_only() {
        let caller = Address::from([0xbb; 20]);
        let tx_hash = B256::from([0xcc; 32]);
        let mut context = context();
        let input = IArbFilteredTransactionsManager::addFilteredTransactionCall { txHash: tx_hash }
            .abi_encode();

        let result = provider_call_with(
            &input,
            caller,
            &mut context,
            false,
            U256::from(1),
            CallScheme::Call,
            ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS,
        );

        assert_eq!(result.result, InstructionResult::Revert);
        assert_eq!(result.gas.spent(), WRAPPER_ACCESS_GAS);
        assert!(!is_filtered(&mut context, tx_hash));
    }

    #[test]
    fn event_burns_gas_before_log() {
        let tx_hash = B256::from([0xdd; 32]);
        let cost = log_gas(1, 0);
        let mut context = context();

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, cost - 1, 0);
            let error = ArbFilteredTransactionsManager::emit(
                &mut storage,
                "FilteredTransactionAdded(bytes32)",
                tx_hash,
            )
            .expect_err("event should run out of gas");
            assert!(error.is_oog());
        }
        assert!(context.journal_mut().take_logs().is_empty());

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, cost, 0);
            ArbFilteredTransactionsManager::emit(
                &mut storage,
                "FilteredTransactionAdded(bytes32)",
                tx_hash,
            )
            .expect("emit filtered transaction event");
            assert_eq!(storage.gas_used, cost);
        }
        assert_eq!(context.journal_mut().take_logs().len(), 1);
    }
}
