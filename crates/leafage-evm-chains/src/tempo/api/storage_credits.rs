//! T7 SSTORE opcode hook for TIP-1060 storage credits.

use alloy_evm::Database;
use revm::{
    context_interface::host::LoadError,
    interpreter::{
        interpreter::EthInterpreter,
        interpreter_types::{InputsTr, RuntimeFlag, StackTr},
        Host, InstructionContext, InstructionResult,
    },
};

use super::{TempoContext, TempoEvm, TempoEvmError};
use crate::tempo::precompile::{
    storage_credits::{
        account_opcode_storage_write, AccountingError, TransientState, STORAGE_CREDIT_VALUE,
    },
    STORAGE_CREDITS_ADDRESS,
};

pub(crate) fn sstore<DB: Database>(
    context: InstructionContext<'_, TempoContext<DB>, EthInterpreter>,
) {
    revm::interpreter::require_non_staticcall!(context.interpreter);
    let Some([index, value]) = StackTr::popn(&mut context.interpreter.stack) else {
        context.interpreter.halt_underflow();
        return;
    };

    let target = context.interpreter.input.target_address();
    if context.interpreter.gas.remaining() <= context.host.gas_params().call_stipend() {
        context
            .interpreter
            .halt(InstructionResult::ReentrancySentryOOG);
        return;
    }

    revm::interpreter::gas!(
        context.interpreter,
        context.host.gas_params().sstore_static_gas()
    );

    let additional_cold_cost = context.host.gas_params().cold_storage_additional_cost();
    let skip_cold = context.interpreter.gas.remaining() < additional_cold_cost;
    let state_load = match context
        .host
        .sstore_skip_cold_load(target, index, value, skip_cold)
    {
        Ok(load) => load,
        Err(LoadError::ColdLoadSkipped) => return context.interpreter.halt_oog(),
        Err(LoadError::DBError) => return context.interpreter.halt_fatal(),
    };

    if let Err(error) = account_opcode_storage_write(
        context.host,
        &mut context.interpreter.gas,
        target,
        &state_load,
    ) {
        match error {
            AccountingError::OutOfGas => context.interpreter.halt_oog(),
            AccountingError::Fatal => context.interpreter.halt_fatal(),
        }
        return;
    }

    revm::interpreter::gas!(
        context.interpreter,
        context
            .host
            .gas_params()
            .sstore_dynamic_gas(true, &state_load.data, state_load.is_cold)
    );
    context.interpreter.gas.record_refund(
        context
            .host
            .gas_params()
            .sstore_refund(true, &state_load.data),
    );
}

/// Settles successful transaction Refund-mode creations against persistent credits.
pub(crate) fn apply_refund<DB: Database, I>(
    evm: &mut TempoEvm<DB, I>,
    gas: &mut revm::interpreter::Gas,
) -> Result<(), TempoEvmError<DB::Error>> {
    use revm::context_interface::{ContextTr, JournalTr};

    let transient_entries: Vec<_> = evm
        .inner
        .ctx
        .journaled_state
        .transient_storage
        .iter()
        .filter_map(|((address, key), value)| {
            (*address == STORAGE_CREDITS_ADDRESS).then_some((*key, *value))
        })
        .collect();

    for (key, _) in &transient_entries {
        evm.inner
            .ctx
            .journaled_state
            .transient_storage
            .remove(&(STORAGE_CREDITS_ADDRESS, *key));
    }

    let mut settled_total = 0u64;
    for (key, word) in transient_entries {
        let state = TransientState::try_from(word).map_err(|error| {
            revm::context_interface::result::EVMError::Custom(error.to_string())
        })?;
        if state.pending_refunds == 0 {
            continue;
        }

        let old_word = evm
            .ctx_mut()
            .journal_mut()
            .sload(STORAGE_CREDITS_ADDRESS, key)?
            .data;
        let balance = u64::try_from(old_word).map_err(|_| {
            revm::context_interface::result::EVMError::Custom(
                "invalid storage credit balance".into(),
            )
        })?;
        let settled = state.pending_refunds.min(balance);
        if settled == 0 {
            continue;
        }

        evm.ctx_mut().journal_mut().sstore(
            STORAGE_CREDITS_ADDRESS,
            key,
            revm::primitives::U256::from(balance - settled),
        )?;
        settled_total = settled_total.saturating_add(settled);
    }

    gas.record_refund(
        settled_total
            .saturating_mul(STORAGE_CREDIT_VALUE)
            .min(i64::MAX as u64) as i64,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_evm::EvmEnv;
    use revm::{
        bytecode::Bytecode,
        context::{BlockEnv, CfgEnv, TxEnv},
        database::{in_memory_db::CacheDB, EmptyDB},
        inspector::NoOpInspector,
        primitives::{Address, TxKind, U256},
        state::AccountInfo,
        ExecuteEvm,
    };

    use crate::tempo::{
        hardfork::TempoHardfork,
        precompile::{
            storage_credits::StorageCredits, storage_types::StorageKey, PATH_USD_ADDRESS,
        },
        tx::TempoTxEnv,
    };

    const T7_TIMESTAMP: u64 = 1_783_605_600;

    fn env() -> EvmEnv<TempoHardfork> {
        let mut cfg = CfgEnv::new_with_spec(TempoHardfork::T7);
        cfg.chain_id = 4217;
        cfg.disable_balance_check = true;
        cfg.disable_eip3607 = true;
        cfg.disable_block_gas_limit = true;
        cfg.disable_base_fee = true;
        let mut block = BlockEnv::default();
        block.timestamp = U256::from(T7_TIMESTAMP);
        block.gas_limit = 10_000_000;
        EvmEnv::new(cfg, block)
    }

    fn tx(contract: Address) -> TempoTxEnv {
        TempoTxEnv {
            base: TxEnv {
                caller: Address::with_last_byte(0xaa),
                kind: TxKind::Call(contract),
                gas_limit: 10_000_000,
                chain_id: Some(4217),
                ..Default::default()
            },
            tempo_fields: None,
            tx_hash: revm::primitives::B256::ZERO,
            unique_tx_identifier: None,
        }
    }

    fn db_with_contract(
        bytecode: Bytecode,
        contract: Address,
        initial_slot: U256,
        credit_balance: u64,
    ) -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            contract,
            AccountInfo {
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
                ..Default::default()
            },
        );
        if !initial_slot.is_zero() {
            db.insert_account_storage(contract, U256::ZERO, initial_slot)
                .unwrap();
        }
        if credit_balance > 0 {
            db.insert_account_storage(
                STORAGE_CREDITS_ADDRESS,
                StorageCredits::slot(contract),
                U256::from(credit_balance),
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn opcode_clear_mints_persistent_credit() {
        let contract = Address::with_last_byte(0xcc);
        // PUSH1 0 (value), PUSH1 0 (slot), SSTORE, STOP.
        let bytecode = Bytecode::new_legacy(vec![0x60, 0, 0x60, 0, 0x55, 0].into());
        let db = db_with_contract(bytecode, contract, U256::ONE, 0);
        let mut evm = TempoEvm::new(env(), db, NoOpInspector, false);

        let result = evm.transact(tx(contract)).unwrap();
        assert!(result.result.is_success(), "{:?}", result.result);
        let credit_slot = result.state[&STORAGE_CREDITS_ADDRESS]
            .storage
            .get(&StorageCredits::slot(contract))
            .unwrap();
        assert_eq!(credit_slot.present_value, U256::ONE);
    }

    #[test]
    fn refund_mode_settles_credit_without_refund_cap() {
        let contract = Address::with_last_byte(0xcd);
        // PUSH1 1 (value), PUSH1 0 (slot), SSTORE, STOP.
        let bytecode = Bytecode::new_legacy(vec![0x60, 1, 0x60, 0, 0x55, 0].into());
        let db = db_with_contract(bytecode, contract, U256::ZERO, 1);
        let mut evm = TempoEvm::new(env(), db, NoOpInspector, false);

        let result = evm.transact(tx(contract)).unwrap();
        assert!(result.result.is_success(), "{:?}", result.result);
        let credit_slot = result.state[&STORAGE_CREDITS_ADDRESS]
            .storage
            .get(&StorageCredits::slot(contract))
            .unwrap();
        assert_eq!(credit_slot.present_value, U256::ZERO);
        assert_eq!(
            result.result.gas().inner_refunded(),
            STORAGE_CREDIT_VALUE,
            "T7 storage-credit refund must not be capped to one fifth of spent gas"
        );
    }

    fn transfer_result(token: Address) -> revm::context_interface::result::ResultAndState {
        let sender = Address::with_last_byte(0xaa);
        let recipient = Address::with_last_byte(0xbb);
        let amount = U256::from(1_000);
        let marker = Bytecode::new_legacy(vec![0xef].into());
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            token,
            AccountInfo {
                code_hash: marker.hash_slow(),
                code: Some(marker),
                nonce: 1,
                ..Default::default()
            },
        );
        db.insert_account_storage(token, U256::from(7), U256::ONE << 160)
            .unwrap();
        db.insert_account_storage(token, U256::from(8), amount + U256::ONE)
            .unwrap();
        db.insert_account_storage(token, sender.mapping_slot(U256::from(9)), amount)
            .unwrap();
        db.insert_account_storage(token, recipient.mapping_slot(U256::from(9)), U256::ONE)
            .unwrap();

        let mut calldata = vec![0xa9, 0x05, 0x9c, 0xbb];
        let mut recipient_word = [0u8; 32];
        recipient_word[12..].copy_from_slice(recipient.as_slice());
        calldata.extend_from_slice(&recipient_word);
        calldata.extend_from_slice(&amount.to_be_bytes::<32>());
        let tx = TempoTxEnv {
            base: TxEnv {
                caller: sender,
                kind: TxKind::Call(token),
                gas_limit: 10_000_000,
                chain_id: Some(4217),
                data: calldata.into(),
                ..Default::default()
            },
            tempo_fields: None,
            tx_hash: revm::primitives::B256::ZERO,
            unique_tx_identifier: None,
        };
        TempoEvm::new(env(), db, NoOpInspector, false)
            .transact(tx)
            .unwrap()
    }

    #[test]
    fn precompile_clear_mints_credit_for_non_fee_token() {
        let token = Address::new({
            let mut bytes = [0u8; 20];
            bytes.copy_from_slice(PATH_USD_ADDRESS.as_slice());
            bytes[19] = 1;
            bytes
        });
        let result = transfer_result(token);
        assert!(result.result.is_success(), "{:?}", result.result);
        let credit = &result.state[&STORAGE_CREDITS_ADDRESS].storage[&StorageCredits::slot(token)];
        assert_eq!(credit.present_value, U256::ONE);
    }

    #[test]
    fn fee_payer_balance_clear_does_not_mint_credit() {
        let result = transfer_result(PATH_USD_ADDRESS);
        assert!(result.result.is_success(), "{:?}", result.result);
        let credit = result
            .state
            .get(&STORAGE_CREDITS_ADDRESS)
            .and_then(|account| account.storage.get(&StorageCredits::slot(PATH_USD_ADDRESS)))
            .map(|slot| slot.present_value)
            .unwrap_or_default();
        assert_eq!(credit, U256::ZERO);
    }
}
