use super::{
    evm::ArcContext,
    native::{
        blocklist_storage_slot, eip7708_transfer_log, is_blocklisted_status, revert_message,
        ERR_BLOCKED_ADDRESS, ERR_SELFDESTRUCTED_BALANCE_INCREASED, ERR_ZERO_ADDRESS,
        NATIVE_COIN_CONTROL_ADDRESS,
    },
    ArcHardfork, ArcHardforkFlags,
};
use alloy_evm::Database;
use revm::{
    context::{ContextTr, Host, JournalTr},
    context_interface::host::LoadError,
    interpreter::{
        instructions::utility::IntoAddress,
        interpreter::EthInterpreter,
        interpreter_action::InterpreterAction,
        interpreter_types::{InputsTr, LoopControl, RuntimeFlag, StackTr},
        require_non_staticcall, Instruction, InstructionContext, InstructionResult, StateLoad,
    },
    primitives::{hardfork::SpecId, Address},
};

#[derive(Clone, Copy)]
enum BlocklistReadPolicy {
    FailOpen,
    FailClosed,
}

#[derive(Clone, Copy)]
enum TargetWarmthPolicy {
    AccessListOnly,
    Transaction,
}

pub(crate) fn arc_selfdestruct_instruction<DB: Database>(
    hardfork_flags: ArcHardforkFlags,
) -> Instruction<EthInterpreter, ArcContext<DB>> {
    if hardfork_flags.is_active(ArcHardfork::Zero8) {
        Instruction::new(arc_selfdestruct_zero8::<DB>, 5_000)
    } else if hardfork_flags.is_active(ArcHardfork::Zero7) {
        Instruction::new(arc_selfdestruct_zero7::<DB>, 5_000)
    } else {
        Instruction::new(arc_selfdestruct::<DB>, 5_000)
    }
}

/// Pre-Zero7 SELFDESTRUCT with fail-open blocklist reads.
pub(crate) fn arc_selfdestruct<DB: Database>(
    context: InstructionContext<'_, ArcContext<DB>, EthInterpreter>,
) {
    arc_selfdestruct_impl(
        context,
        BlocklistReadPolicy::FailOpen,
        TargetWarmthPolicy::AccessListOnly,
    );
}

/// Zero7+ SELFDESTRUCT with fail-closed blocklist reads.
pub(crate) fn arc_selfdestruct_zero7<DB: Database>(
    context: InstructionContext<'_, ArcContext<DB>, EthInterpreter>,
) {
    arc_selfdestruct_impl(
        context,
        BlocklistReadPolicy::FailClosed,
        TargetWarmthPolicy::AccessListOnly,
    );
}

/// Zero8+ SELFDESTRUCT with transaction-wide target warmth.
pub(crate) fn arc_selfdestruct_zero8<DB: Database>(
    context: InstructionContext<'_, ArcContext<DB>, EthInterpreter>,
) {
    arc_selfdestruct_impl(
        context,
        BlocklistReadPolicy::FailClosed,
        TargetWarmthPolicy::Transaction,
    );
}

fn arc_selfdestruct_impl<DB: Database>(
    mut context: InstructionContext<'_, ArcContext<DB>, EthInterpreter>,
    blocklist_read_policy: BlocklistReadPolicy,
    target_warmth_policy: TargetWarmthPolicy,
) {
    require_non_staticcall!(context.interpreter);
    let Some([target]) = StackTr::popn(&mut context.interpreter.stack) else {
        context.interpreter.halt_underflow();
        return;
    };
    let target = target.into_address();
    let source = context.interpreter.input.target_address();
    let spec = context.interpreter.runtime_flag.spec_id();
    let cold_load_gas = context.host.gas_params().selfdestruct_cold_cost();
    let skip_cold_load = context.interpreter.gas.remaining() < cold_load_gas;

    let source_balance = context.host.balance(source);
    let target_cold_override = match source_balance.as_ref() {
        Some(balance) if !balance.is_zero() => {
            if target.is_zero() {
                revert(&mut context, ERR_ZERO_ADDRESS);
                return;
            }
            let Ok(is_target_cold) = check_accounts(
                &mut context,
                source,
                target,
                skip_cold_load,
                blocklist_read_policy,
                target_warmth_policy,
            ) else {
                return;
            };
            Some(is_target_cold)
        }
        Some(_) => None,
        None => {
            context
                .interpreter
                .halt(InstructionResult::FatalExternalError);
            return;
        }
    };

    let result = match context
        .host
        .selfdestruct(source, target, skip_cold_load)
        .map(|result| StateLoad {
            data: result.data,
            is_cold: target_cold_override.unwrap_or(result.is_cold),
        }) {
        Ok(result) => result,
        Err(LoadError::ColdLoadSkipped) => {
            context.interpreter.halt_oog();
            return;
        }
        Err(LoadError::DBError) => {
            context.interpreter.halt_fatal();
            return;
        }
    };

    if let Some(balance) = source_balance {
        if !balance.is_zero() {
            context
                .host
                .log(eip7708_transfer_log(source, target, balance.data));
        }
    }

    let should_charge_topup = if spec.is_enabled_in(SpecId::SPURIOUS_DRAGON) {
        result.had_value && !result.target_exists
    } else {
        !result.target_exists
    };
    let gas_cost = context
        .host
        .gas_params()
        .selfdestruct_cost(should_charge_topup, result.is_cold);
    if !context.interpreter.gas.record_cost(gas_cost) {
        context.interpreter.halt_oog();
        return;
    }

    if !result.previously_destroyed {
        context
            .interpreter
            .gas
            .record_refund(context.host.gas_params().selfdestruct_refund());
    }
    context.interpreter.halt(InstructionResult::SelfDestruct);
}

fn is_blocklisted<DB: Database>(
    context: &mut InstructionContext<'_, ArcContext<DB>, EthInterpreter>,
    address: Address,
    policy: BlocklistReadPolicy,
) -> Result<bool, LoadError> {
    let slot = blocklist_storage_slot(address);
    match policy {
        BlocklistReadPolicy::FailOpen => Ok(context
            .host
            .sload(NATIVE_COIN_CONTROL_ADDRESS, slot)
            .is_some_and(|value| is_blocklisted_status(value.data))),
        BlocklistReadPolicy::FailClosed => {
            let value =
                match context
                    .host
                    .sload_skip_cold_load(NATIVE_COIN_CONTROL_ADDRESS, slot, false)
                {
                    Ok(value) => value,
                    Err(LoadError::ColdLoadSkipped) => {
                        let ncc_is_cold = context
                            .host
                            .load_account_info_skip_cold_load(
                                NATIVE_COIN_CONTROL_ADDRESS,
                                false,
                                false,
                            )?
                            .is_cold;
                        debug_assert!(!ncc_is_cold, "NativeCoinControl must be preloaded");
                        context.host.sload_skip_cold_load(
                            NATIVE_COIN_CONTROL_ADDRESS,
                            slot,
                            false,
                        )?
                    }
                    Err(LoadError::DBError) => return Err(LoadError::DBError),
                };
            Ok(is_blocklisted_status(value.data))
        }
    }
}

fn check_accounts<DB: Database>(
    context: &mut InstructionContext<'_, ArcContext<DB>, EthInterpreter>,
    source: Address,
    target: Address,
    skip_cold_load: bool,
    blocklist_read_policy: BlocklistReadPolicy,
    target_warmth_policy: TargetWarmthPolicy,
) -> Result<bool, ()> {
    if source == target {
        context.interpreter.halt(InstructionResult::Revert);
        return Err(());
    }
    let target_blocklisted = match is_blocklisted(context, target, blocklist_read_policy) {
        Ok(is_blocklisted) => is_blocklisted,
        Err(_) => {
            context.interpreter.halt_fatal();
            return Err(());
        }
    };
    if target_blocklisted {
        revert(context, ERR_BLOCKED_ADDRESS);
        return Err(());
    }
    let source_blocklisted = match is_blocklisted(context, source, blocklist_read_policy) {
        Ok(is_blocklisted) => is_blocklisted,
        Err(_) => {
            context.interpreter.halt_fatal();
            return Err(());
        }
    };
    if source_blocklisted {
        revert(context, ERR_BLOCKED_ADDRESS);
        return Err(());
    }

    let is_cold = match target_warmth_policy {
        TargetWarmthPolicy::AccessListOnly => {
            if context
                .host
                .journal_mut()
                .warm_addresses
                .check_is_cold::<DB::Error>(&target, skip_cold_load)
                .is_err()
            {
                context.interpreter.halt_oog();
                return Err(());
            }
            match context.host.journal_mut().load_account(target) {
                Ok(account) => account.is_cold,
                Err(_) => {
                    context.interpreter.halt_fatal();
                    return Err(());
                }
            }
        }
        TargetWarmthPolicy::Transaction => {
            // revm 36's mutable helper cannot return ColdLoadSkipped without panicking. Load
            // through Host first, then use the now-warm journal account for the status check.
            match context
                .host
                .load_account_info_skip_cold_load(target, false, skip_cold_load)
            {
                Ok(account) => account.is_cold,
                Err(LoadError::ColdLoadSkipped) => {
                    context.interpreter.halt_oog();
                    return Err(());
                }
                Err(LoadError::DBError) => {
                    context.interpreter.halt_fatal();
                    return Err(());
                }
            }
        }
    };

    match context.host.journal_mut().load_account(target) {
        Ok(account) if account.is_selfdestructed() => {
            revert(context, ERR_SELFDESTRUCTED_BALANCE_INCREASED);
            Err(())
        }
        Ok(_) => Ok(is_cold),
        Err(_) => {
            context.interpreter.halt_fatal();
            Err(())
        }
    }
}

fn revert<DB: Database>(
    context: &mut InstructionContext<'_, ArcContext<DB>, EthInterpreter>,
    message: &str,
) {
    context
        .interpreter
        .bytecode
        .set_action(InterpreterAction::new_return(
            InstructionResult::Revert,
            revert_message(message),
            context.interpreter.gas,
        ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arc::{
        handler::ArcHandler, ArcChainConfig, ArcEvm, ArcEvmFactory, ArcForkActivation,
        ArcHardforkSchedule, ARC_MAINNET_CHAIN_ID,
    };
    use alloy::primitives::{address, B256, U256};
    use alloy_evm::EvmEnv;
    use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId};
    use revm::{
        bytecode::Bytecode,
        context::{result::EVMError, ContextSetters, TxEnv},
        context_interface::{
            block::BlobExcessGasAndPrice, journaled_state::account::JournaledAccountTr,
        },
        database::InMemoryDB,
        handler::{EthFrame, Handler, MainnetHandler},
        inspector::NoOpInspector,
        interpreter::{
            interpreter::{InputsImpl, Interpreter, RuntimeFlags},
            interpreter_action::InterpreterAction,
            Gas, InterpreterResult,
        },
        primitives::TxKind,
        state::AccountInfo,
        Database as RevmDatabase, ExecuteEvm,
    };
    use std::{convert::Infallible, error::Error, fmt};

    const SOURCE: Address = address!("1000000000000000000000000000000000000001");
    const TARGET: Address = address!("2000000000000000000000000000000000000002");
    const STATIC_GAS_COST: u64 = 5_000;

    struct HostTestEnv<DB: Database> {
        host: ArcContext<DB>,
    }

    impl<DB: Database> HostTestEnv<DB> {
        fn new(db: DB) -> Self {
            let mut host = ArcContext::new(db, MainnetSpecId::OSAKA);
            host.cfg.amsterdam_eip7708_disabled = true;
            host.cfg.amsterdam_eip7708_delayed_burn_disabled = true;
            host.journaled_state.set_eip7708_config(true, true);
            Self { host }
        }

        fn set_balance(&mut self, address: Address, balance: U256) {
            self.host
                .journal_mut()
                .load_account_mut_optional_code(address, false)
                .unwrap()
                .data
                .set_balance(balance);
        }

        fn balance(&mut self, address: Address) -> U256 {
            self.host
                .journal_mut()
                .load_account(address)
                .unwrap()
                .info
                .balance
        }

        fn simulate(
            &mut self,
            source: Address,
            target: Address,
            gas_limit: u64,
        ) -> InterpreterResult {
            self.simulate_with_ncc_preload(source, target, gas_limit, true)
        }

        fn simulate_with_ncc_preload(
            &mut self,
            source: Address,
            target: Address,
            gas_limit: u64,
            preload_ncc: bool,
        ) -> InterpreterResult {
            self.simulate_instruction(
                source,
                target,
                gas_limit,
                preload_ncc,
                Instruction::new(arc_selfdestruct::<DB>, STATIC_GAS_COST),
            )
        }

        fn simulate_with_flags(
            &mut self,
            source: Address,
            target: Address,
            gas_limit: u64,
            hardfork_flags: ArcHardforkFlags,
        ) -> InterpreterResult {
            self.simulate_instruction(
                source,
                target,
                gas_limit,
                true,
                arc_selfdestruct_instruction::<DB>(hardfork_flags),
            )
        }

        fn simulate_instruction(
            &mut self,
            source: Address,
            target: Address,
            gas_limit: u64,
            preload_ncc: bool,
            instruction: Instruction<EthInterpreter, ArcContext<DB>>,
        ) -> InterpreterResult {
            if preload_ncc {
                self.host
                    .journal_mut()
                    .load_account(NATIVE_COIN_CONTROL_ADDRESS)
                    .unwrap();
            }

            let mut interpreter = Interpreter::<EthInterpreter> {
                gas: Gas::new(gas_limit),
                input: InputsImpl {
                    target_address: source,
                    caller_address: source,
                    ..Default::default()
                },
                runtime_flag: RuntimeFlags {
                    spec_id: SpecId::OSAKA,
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(interpreter
                .stack
                .push(U256::from_be_slice(target.into_word().as_ref())));
            assert!(interpreter.gas.record_cost(STATIC_GAS_COST));

            instruction.execute(InstructionContext {
                interpreter: &mut interpreter,
                host: &mut self.host,
            });
            match interpreter.take_next_action() {
                InterpreterAction::Return(result) => result,
                _ => panic!("SELFDESTRUCT must halt the interpreter"),
            }
        }
    }

    fn db_with_source_balance(balance: U256) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            SOURCE,
            AccountInfo {
                balance,
                ..Default::default()
            },
        );
        db
    }

    fn blocklist_in_db(db: &mut InMemoryDB, address: Address) {
        db.insert_account_storage(
            NATIVE_COIN_CONTROL_ADDRESS,
            blocklist_storage_slot(address),
            U256::ONE,
        )
        .unwrap();
    }

    fn hardfork_flags(zero7: bool, zero8: bool) -> ArcHardforkFlags {
        let activation = |active| {
            if active {
                ArcForkActivation::Block(0)
            } else {
                ArcForkActivation::Never
            }
        };
        ArcHardforkSchedule::new(
            ArcForkActivation::Never,
            ArcForkActivation::Never,
            ArcForkActivation::Never,
            ArcForkActivation::Never,
            activation(zero7),
            activation(zero8),
        )
        .flags_at(0, 0)
    }

    fn evm_env() -> EvmEnv<MainnetSpecId> {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
        cfg.chain_id = ARC_MAINNET_CHAIN_ID;
        EvmEnv::new(
            cfg,
            BlockEnv {
                number: U256::ONE,
                timestamp: U256::ONE,
                gas_limit: 30_000_000,
                prevrandao: Some(B256::ZERO),
                blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
                    excess_blob_gas: 0,
                    blob_gasprice: 1,
                }),
                ..Default::default()
            },
        )
    }

    fn selfdestruct_code(target: Address) -> (Bytecode, B256) {
        let mut raw = vec![revm::bytecode::opcode::PUSH20];
        raw.extend_from_slice(target.as_slice());
        raw.push(revm::bytecode::opcode::SELFDESTRUCT);
        let raw: alloy::primitives::Bytes = raw.into();
        let hash = alloy::primitives::keccak256(&raw);
        (Bytecode::new_raw(raw), hash)
    }

    fn value_call(caller: Address, target: Address, gas_limit: u64) -> TxEnv {
        TxEnv {
            caller,
            kind: TxKind::Call(target),
            gas_limit,
            chain_id: Some(ARC_MAINNET_CHAIN_ID),
            ..Default::default()
        }
    }

    #[test]
    fn selfdestruct_nonzero_value_has_arc_gas_and_one_manual_log() {
        let amount = U256::from(42);
        let mut env = HostTestEnv::new(db_with_source_balance(amount));

        let result = env.simulate(SOURCE, TARGET, u64::MAX);

        assert_eq!(result.result, InstructionResult::SelfDestruct);
        assert_eq!(result.gas.spent(), 32_600);
        assert_eq!(result.gas.refunded(), 0);
        assert_eq!(env.balance(SOURCE), U256::ZERO);
        assert_eq!(env.balance(TARGET), amount);
        assert_eq!(
            env.host.journal_mut().take_logs(),
            vec![eip7708_transfer_log(SOURCE, TARGET, amount)]
        );
    }

    #[test]
    fn selfdestruct_zero_balance_and_warm_target_preserve_mainnet_gas() {
        let mut zero = HostTestEnv::new(InMemoryDB::default());
        let zero_result = zero.simulate(SOURCE, TARGET, u64::MAX);
        assert_eq!(zero_result.result, InstructionResult::SelfDestruct);
        assert_eq!(zero_result.gas.spent(), 7_600);
        assert_eq!(zero_result.gas.refunded(), 0);
        assert!(zero.host.journal_mut().take_logs().is_empty());

        let mut warm = HostTestEnv::new(db_with_source_balance(U256::from(42)));
        warm.set_balance(TARGET, U256::ONE);
        let warm_result = warm.simulate(SOURCE, TARGET, u64::MAX);
        assert_eq!(warm_result.result, InstructionResult::SelfDestruct);
        assert_eq!(warm_result.gas.spent(), STATIC_GAS_COST);
        assert_eq!(warm_result.gas.refunded(), 0);
    }

    #[test]
    fn selfdestruct_low_gas_cold_target_halts_out_of_gas() {
        let initial_gas = STATIC_GAS_COST + 100;
        let mut env = HostTestEnv::new(db_with_source_balance(U256::ONE));

        let result = env.simulate(SOURCE, TARGET, initial_gas);

        assert_eq!(result.result, InstructionResult::OutOfGas);
        assert_eq!(result.gas.spent(), initial_gas);
        assert_eq!(env.balance(SOURCE), U256::ONE);
        assert!(env.host.journal_mut().take_logs().is_empty());
    }

    #[test]
    fn zero8_uses_transaction_warmth_for_selfdestruct_target() {
        let initial_gas = STATIC_GAS_COST + 100;

        for flags in [hardfork_flags(false, false), hardfork_flags(true, false)] {
            let mut env = HostTestEnv::new(db_with_source_balance(U256::ONE));
            env.set_balance(TARGET, U256::ONE);

            let result = env.simulate_with_flags(SOURCE, TARGET, initial_gas, flags);

            assert_eq!(result.result, InstructionResult::OutOfGas);
            assert_eq!(env.balance(SOURCE), U256::ONE);
            assert_eq!(env.balance(TARGET), U256::ONE);
        }

        for flags in [hardfork_flags(false, true), hardfork_flags(true, true)] {
            let mut env = HostTestEnv::new(db_with_source_balance(U256::ONE));
            env.set_balance(TARGET, U256::ONE);

            let result = env.simulate_with_flags(SOURCE, TARGET, initial_gas, flags);

            assert_eq!(result.result, InstructionResult::SelfDestruct);
            assert_eq!(result.gas.spent(), STATIC_GAS_COST);
            assert_eq!(env.balance(SOURCE), U256::ZERO);
            assert_eq!(env.balance(TARGET), U256::from(2));
        }
    }

    #[test]
    fn zero8_does_not_carry_target_warmth_across_transactions() {
        let initial_gas = STATIC_GAS_COST + 100;
        let mut env = HostTestEnv::new(db_with_source_balance(U256::ONE));
        env.set_balance(TARGET, U256::ONE);
        env.host.journal_mut().commit_tx();

        let result =
            env.simulate_with_flags(SOURCE, TARGET, initial_gas, hardfork_flags(false, true));

        assert_eq!(result.result, InstructionResult::OutOfGas);
        assert_eq!(env.balance(SOURCE), U256::ONE);
        assert_eq!(env.balance(TARGET), U256::ONE);
    }

    #[test]
    fn selfdestruct_rejection_order_is_zero_self_target_then_source() {
        let amount = U256::from(42);
        let mut zero_db = db_with_source_balance(amount);
        blocklist_in_db(&mut zero_db, SOURCE);
        let mut zero = HostTestEnv::new(zero_db);
        let zero_result = zero.simulate(SOURCE, Address::ZERO, u64::MAX);
        assert_eq!(zero_result.result, InstructionResult::Revert);
        assert_eq!(zero_result.output, revert_message(ERR_ZERO_ADDRESS));

        let mut self_db = db_with_source_balance(amount);
        blocklist_in_db(&mut self_db, SOURCE);
        let mut same = HostTestEnv::new(self_db);
        let same_result = same.simulate(SOURCE, SOURCE, u64::MAX);
        assert_eq!(same_result.result, InstructionResult::Revert);
        assert!(same_result.output.is_empty());

        let mut both_db = db_with_source_balance(amount);
        blocklist_in_db(&mut both_db, SOURCE);
        blocklist_in_db(&mut both_db, TARGET);
        let mut both = HostTestEnv::new(both_db);
        let blocked_result = both.simulate(SOURCE, TARGET, u64::MAX);
        assert_eq!(blocked_result.result, InstructionResult::Revert);
        assert_eq!(blocked_result.output, revert_message(ERR_BLOCKED_ADDRESS));
        assert!(
            !both
                .host
                .journal_mut()
                .sload(NATIVE_COIN_CONTROL_ADDRESS, blocklist_storage_slot(TARGET))
                .unwrap()
                .is_cold
        );
        assert!(
            both.host
                .journal_mut()
                .sload(NATIVE_COIN_CONTROL_ADDRESS, blocklist_storage_slot(SOURCE))
                .unwrap()
                .is_cold
        );
    }

    #[test]
    fn selfdestruct_rejects_an_already_destroyed_target() {
        let mut env = HostTestEnv::new(db_with_source_balance(U256::from(42)));
        env.host.journal_mut().load_account(TARGET).unwrap();
        env.host
            .journaled_state
            .state
            .get_mut(&TARGET)
            .unwrap()
            .mark_selfdestruct();

        let result = env.simulate(SOURCE, TARGET, u64::MAX);

        assert_eq!(result.result, InstructionResult::Revert);
        assert_eq!(
            result.output,
            revert_message(ERR_SELFDESTRUCTED_BALANCE_INCREASED)
        );
        assert_eq!(env.balance(SOURCE), U256::from(42));
        assert!(env.host.journal_mut().take_logs().is_empty());
    }

    #[test]
    fn selfdestruct_does_not_enable_revm_delayed_burn_tracking() {
        let amount = U256::from(42);
        let mut env = HostTestEnv::new(db_with_source_balance(amount));
        env.host.journal_mut().load_account(SOURCE).unwrap();
        env.host
            .journaled_state
            .state
            .get_mut(&SOURCE)
            .unwrap()
            .mark_created_locally();

        let result = env.simulate(SOURCE, TARGET, u64::MAX);

        assert_eq!(result.result, InstructionResult::SelfDestruct);
        assert!(env.host.journaled_state.selfdestructed_addresses.is_empty());
        assert_eq!(env.host.journaled_state.logs.len(), 1);
    }

    #[test]
    fn selfdestruct_obeys_eip6780_created_local_deletion_boundary() {
        let amount = U256::from(42);
        let (code, code_hash) = selfdestruct_code(TARGET);
        let mut old_db = db_with_source_balance(amount);
        old_db.insert_account_info(
            SOURCE,
            AccountInfo {
                balance: amount,
                nonce: 1,
                code_hash,
                code: Some(code.clone()),
                ..Default::default()
            },
        );
        let mut old = HostTestEnv::new(old_db);

        let old_result = old.simulate(SOURCE, TARGET, u64::MAX);

        assert_eq!(old_result.result, InstructionResult::SelfDestruct);
        let old_account = old.host.journaled_state.state.get(&SOURCE).unwrap();
        assert_eq!(old_account.info.balance, U256::ZERO);
        assert_eq!(old_account.info.code_hash, code_hash);
        assert_eq!(old_account.info.code.as_ref(), Some(&code));
        assert!(!old_account.is_selfdestructed());

        let mut created_db = db_with_source_balance(amount);
        created_db.insert_account_info(
            SOURCE,
            AccountInfo {
                balance: amount,
                nonce: 1,
                code_hash,
                code: Some(code),
                ..Default::default()
            },
        );
        let mut created = HostTestEnv::new(created_db);
        created.host.journal_mut().load_account(SOURCE).unwrap();
        created
            .host
            .journaled_state
            .state
            .get_mut(&SOURCE)
            .unwrap()
            .mark_created_locally();

        let created_result = created.simulate(SOURCE, TARGET, u64::MAX);

        assert_eq!(created_result.result, InstructionResult::SelfDestruct);
        assert!(created
            .host
            .journaled_state
            .state
            .get(&SOURCE)
            .unwrap()
            .is_selfdestructed());
    }

    #[test]
    fn selfdestruct_unpreheated_ncc_cold_load_skip_succeeds_in_a_full_frame() {
        let sender = Address::with_last_byte(0x71);
        let (code, code_hash) = selfdestruct_code(TARGET);
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        db.insert_account_info(
            SOURCE,
            AccountInfo {
                balance: U256::from(42),
                nonce: 1,
                code_hash,
                code: Some(code),
                ..Default::default()
            },
        );
        let mut evm = ArcEvmFactory::new(ArcChainConfig::mainnet())
            .create(evm_env(), db, NoOpInspector {})
            .unwrap();
        assert!(!evm
            .ctx()
            .journaled_state
            .state
            .contains_key(&NATIVE_COIN_CONTROL_ADDRESS));
        evm.ctx_mut().set_tx(value_call(sender, SOURCE, 100_000));
        let mut handler: MainnetHandler<
            ArcEvm<InMemoryDB, NoOpInspector>,
            EVMError<Infallible>,
            EthFrame,
        > = MainnetHandler::default();

        let result = handler.run(&mut evm).unwrap();
        let state = evm.finalize();

        assert!(result.is_success());
        assert_eq!(result.logs().len(), 1);
        assert_eq!(state.get(&SOURCE).unwrap().info.balance, U256::ZERO);
        assert_eq!(state.get(&TARGET).unwrap().info.balance, U256::from(42));
    }

    #[derive(Debug)]
    struct InjectedStorageError;

    impl fmt::Display for InjectedStorageError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("injected native coin control storage error")
        }
    }

    impl Error for InjectedStorageError {}

    impl revm::database_interface::DBErrorMarker for InjectedStorageError {}

    #[derive(Clone, Debug)]
    struct SelectiveFailingStorageDb {
        inner: InMemoryDB,
        failing_ncc_slots: [U256; 2],
        ncc_reads: Vec<U256>,
    }

    impl SelectiveFailingStorageDb {
        fn new(inner: InMemoryDB) -> Self {
            Self {
                inner,
                failing_ncc_slots: [
                    blocklist_storage_slot(TARGET),
                    blocklist_storage_slot(SOURCE),
                ],
                ncc_reads: Vec::new(),
            }
        }
    }

    fn infallible<T>(result: Result<T, Infallible>) -> T {
        match result {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }

    impl RevmDatabase for SelectiveFailingStorageDb {
        type Error = InjectedStorageError;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(infallible(self.inner.basic(address)))
        }

        fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(infallible(self.inner.code_by_hash(code_hash)))
        }

        fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
            if address == NATIVE_COIN_CONTROL_ADDRESS {
                self.ncc_reads.push(index);
                if self.failing_ncc_slots.contains(&index) {
                    return Err(InjectedStorageError);
                }
            }
            Ok(infallible(self.inner.storage(address, index)))
        }

        fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
            Ok(infallible(self.inner.block_hash(number)))
        }
    }

    #[test]
    fn selfdestruct_db_error_continues_locally_but_records_error() {
        let db = SelectiveFailingStorageDb::new(db_with_source_balance(U256::from(42)));
        let mut env = HostTestEnv::new(db);

        let result = env.simulate(SOURCE, TARGET, u64::MAX);

        assert_eq!(result.result, InstructionResult::SelfDestruct);
        assert_eq!(env.balance(SOURCE), U256::ZERO);
        assert_eq!(env.balance(TARGET), U256::from(42));
        assert_eq!(
            env.host.journaled_state.logs,
            vec![eip7708_transfer_log(SOURCE, TARGET, U256::from(42))]
        );
        assert!(
            env.host.error.is_err(),
            "Host::sload records the injected DB error"
        );
        assert_eq!(
            env.host.journaled_state.db().ncc_reads,
            [
                blocklist_storage_slot(TARGET),
                blocklist_storage_slot(SOURCE),
            ]
        );
    }

    #[test]
    fn zero7_and_zero8_fail_closed_on_selfdestruct_blocklist_db_error() {
        for flags in [hardfork_flags(true, false), hardfork_flags(false, true)] {
            let amount = U256::from(42);
            let db = SelectiveFailingStorageDb::new(db_with_source_balance(amount));
            let mut env = HostTestEnv::new(db);

            let result = env.simulate_with_flags(SOURCE, TARGET, u64::MAX, flags);

            assert_eq!(result.result, InstructionResult::FatalExternalError);
            assert_eq!(env.balance(SOURCE), amount);
            assert_eq!(env.balance(TARGET), U256::ZERO);
            assert!(env.host.journaled_state.logs.is_empty());
            assert!(env.host.error.is_err());
            assert_eq!(
                env.host.journaled_state.db().ncc_reads,
                [blocklist_storage_slot(TARGET)]
            );
        }
    }

    #[test]
    fn native_coin_control_db_error_fails_the_full_handler_and_discards_state() {
        let sender = Address::with_last_byte(0x72);
        let amount = U256::from(42);
        let (code, code_hash) = selfdestruct_code(TARGET);
        let mut inner = InMemoryDB::default();
        inner.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        inner.insert_account_info(
            SOURCE,
            AccountInfo {
                balance: amount,
                nonce: 1,
                code_hash,
                code: Some(code.clone()),
                ..Default::default()
            },
        );
        let db = SelectiveFailingStorageDb::new(inner);
        let external_api_db = db.clone();
        let mut evm = ArcEvmFactory::new(ArcChainConfig::mainnet())
            .create(evm_env(), db, NoOpInspector {})
            .unwrap();
        evm.ctx_mut().set_tx(value_call(sender, SOURCE, 100_000));
        let mut handler = ArcHandler::new(evm.execution_spec().arc_flags);

        let init_and_floor_gas = handler.validate(&mut evm).unwrap();
        let eip7702_refund = handler.pre_execution(&mut evm).unwrap() as i64;
        assert_eq!(
            evm.ctx().journaled_state.db().ncc_reads,
            [blocklist_storage_slot(sender)],
            "the top-level sender blocklist read must succeed before execution"
        );

        let mut frame_result = handler.execution(&mut evm, &init_and_floor_gas).unwrap();

        assert_eq!(
            evm.ctx().journaled_state.db().ncc_reads,
            [
                blocklist_storage_slot(sender),
                blocklist_storage_slot(TARGET),
                blocklist_storage_slot(SOURCE),
            ],
            "the injected errors must come from SELFDESTRUCT's target/source checks"
        );
        assert!(evm.ctx().error.is_err());
        assert_eq!(
            evm.ctx()
                .journaled_state
                .state
                .get(&SOURCE)
                .unwrap()
                .info
                .balance,
            U256::ZERO
        );
        assert_eq!(
            evm.ctx()
                .journaled_state
                .state
                .get(&TARGET)
                .unwrap()
                .info
                .balance,
            amount
        );
        assert_eq!(
            evm.ctx().journaled_state.logs,
            vec![eip7708_transfer_log(SOURCE, TARGET, amount)]
        );

        let result_gas = handler
            .post_execution(
                &mut evm,
                &mut frame_result,
                init_and_floor_gas,
                eip7702_refund,
            )
            .unwrap();
        let error = handler
            .execution_result(&mut evm, frame_result, result_gas)
            .unwrap_err();
        assert!(matches!(error, EVMError::Database(InjectedStorageError)));

        let error = handler.catch_error(&mut evm, error).unwrap_err();

        assert!(matches!(error, EVMError::Database(InjectedStorageError)));
        assert!(evm.ctx().journaled_state.logs.is_empty());
        assert!(
            evm.ctx()
                .journaled_state
                .state
                .values()
                .all(|account| !account.is_touched() && !account.is_selfdestructed()),
            "discard_tx may cache loaded accounts but must leave no dirty state"
        );
        let reverted_source = evm.ctx().journaled_state.state.get(&SOURCE).unwrap();
        assert_eq!(reverted_source.info.balance, amount);
        assert_eq!(reverted_source.info.code_hash, code_hash);
        assert_eq!(reverted_source.info.code.as_ref(), Some(&code));
        if let Some(reverted_target) = evm.ctx().journaled_state.state.get(&TARGET) {
            assert_eq!(reverted_target.info.balance, U256::ZERO);
        }

        let db = evm.ctx_mut().journal_mut().db_mut();
        let source = db.basic(SOURCE).unwrap().unwrap();
        assert_eq!(source.balance, amount);
        assert_eq!(source.code_hash, code_hash);
        assert_eq!(source.code.as_ref(), Some(&code));
        assert!(db.basic(TARGET).unwrap().is_none());

        let discarded_state = evm.finalize();
        assert!(discarded_state
            .values()
            .all(|account| !account.is_touched() && !account.is_selfdestructed()));
        assert!(evm.ctx().journaled_state.state.is_empty());
        assert!(evm.ctx().journaled_state.logs.is_empty());

        let mut external_api_evm = ArcEvmFactory::new(ArcChainConfig::mainnet())
            .create(evm_env(), external_api_db, NoOpInspector {})
            .unwrap();
        let error = external_api_evm
            .transact(value_call(sender, SOURCE, 100_000))
            .unwrap_err();
        assert!(matches!(error, EVMError::Database(InjectedStorageError)));
        assert_eq!(
            external_api_evm.ctx().journaled_state.db().ncc_reads,
            [
                blocklist_storage_slot(sender),
                blocklist_storage_slot(TARGET),
                blocklist_storage_slot(SOURCE),
            ]
        );
        assert!(external_api_evm.ctx().journaled_state.state.is_empty());
        assert!(external_api_evm.ctx().journaled_state.logs.is_empty());
    }
}
