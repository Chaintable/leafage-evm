use super::{
    evm::{ArcContext, ArcEvm},
    native::{
        blocklist_storage_slot, is_blocklisted_status, ERR_BLOCKED_ADDRESS,
        NATIVE_COIN_CONTROL_ADDRESS,
    },
    ArcHardforkFlags,
};
use alloy_evm::Database;
use leafage_evm_types::{Address, U256};
use revm::{
    context::{
        result::{EVMError, HaltReason, InvalidTransaction},
        Block, ContextTr, JournalTr, Transaction,
    },
    handler::{EthFrame, EvmTr, FrameTr, Handler, MainnetHandler},
    inspector::{Inspector, InspectorHandler},
    interpreter::interpreter::EthInterpreter,
};

/// Arc transaction handler shared by normal and inspected execution.
pub struct ArcHandler<DB: revm::Database, I> {
    mainnet: MainnetHandler<ArcEvm<DB, I>, EVMError<DB::Error>, EthFrame>,
    _hardfork_flags: ArcHardforkFlags,
}

impl<DB: Database, I> ArcHandler<DB, I> {
    pub fn new(hardfork_flags: ArcHardforkFlags) -> Self {
        Self {
            mainnet: MainnetHandler::default(),
            _hardfork_flags: hardfork_flags,
        }
    }

    fn is_address_blocklisted(
        &self,
        evm: &mut ArcEvm<DB, I>,
        address: Address,
    ) -> Result<bool, EVMError<DB::Error>> {
        let state_load = evm
            .ctx_mut()
            .journal_mut()
            .sload(NATIVE_COIN_CONTROL_ADDRESS, blocklist_storage_slot(address))?;
        Ok(is_blocklisted_status(state_load.data))
    }

    fn check_transaction_blocklist(
        &self,
        evm: &mut ArcEvm<DB, I>,
        caller: Address,
        kind: revm::primitives::TxKind,
        value: U256,
    ) -> Result<(), EVMError<DB::Error>> {
        if self.is_address_blocklisted(evm, caller)? {
            return Err(InvalidTransaction::Str(ERR_BLOCKED_ADDRESS.into()).into());
        }
        if let revm::primitives::TxKind::Call(to) = kind {
            if !value.is_zero() && self.is_address_blocklisted(evm, to)? {
                return Err(InvalidTransaction::Str(ERR_BLOCKED_ADDRESS.into()).into());
            }
        }
        Ok(())
    }
}

impl<DB: Database, I> Handler for ArcHandler<DB, I> {
    type Evm = ArcEvm<DB, I>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;

    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        let (caller, kind, value) = {
            let ctx = evm.ctx();
            let tx = ctx.tx();
            (tx.caller(), tx.kind(), tx.value())
        };

        evm.ctx_mut()
            .journal_mut()
            .load_account(NATIVE_COIN_CONTROL_ADDRESS)?;
        self.check_transaction_blocklist(evm, caller, kind, value)?;
        self.mainnet.pre_execution(evm)
    }

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let (beneficiary, effective_gas_price) = {
            let ctx = evm.ctx();
            let basefee = ctx.block().basefee() as u128;
            (
                ctx.block().beneficiary(),
                ctx.tx().effective_gas_price(basefee),
            )
        };
        let total_fee = U256::from(effective_gas_price) * U256::from(exec_result.gas().used());

        evm.ctx_mut()
            .journal_mut()
            .balance_incr(beneficiary, total_fee)
            .map_err(Into::into)
    }
}

impl<DB, I> InspectorHandler for ArcHandler<DB, I>
where
    DB: Database,
    I: Inspector<ArcContext<DB>, EthInterpreter>,
{
    type IT = EthInterpreter;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arc::{ArcChainConfig, ArcEvmFactory, ARC_MAINNET_CHAIN_ID};
    use alloy::primitives::{Address, B256};
    use alloy_evm::EvmEnv;
    use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId};
    use revm::{
        context::{ContextSetters, TxEnv},
        database::InMemoryDB,
        handler::FrameResult,
        inspector::NoOpInspector,
        interpreter::{CallOutcome, Gas, InstructionResult, InterpreterResult},
        primitives::TxKind,
        state::AccountInfo,
    };

    fn env(beneficiary: Address) -> EvmEnv<MainnetSpecId> {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
        cfg.chain_id = ARC_MAINNET_CHAIN_ID;
        EvmEnv::new(
            cfg,
            BlockEnv {
                beneficiary,
                number: U256::ONE,
                timestamp: U256::ONE,
                gas_limit: 30_000_000,
                basefee: 7,
                prevrandao: Some(B256::ZERO),
                ..Default::default()
            },
        )
    }

    fn evm_with_balance(caller: Address, balance: U256) -> ArcEvm<InMemoryDB, NoOpInspector> {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            caller,
            AccountInfo {
                balance,
                ..Default::default()
            },
        );
        ArcEvmFactory::new(ArcChainConfig::mainnet())
            .create(env(Address::with_last_byte(9)), db, NoOpInspector {})
            .unwrap()
    }

    fn value_call(caller: Address, to: Address, value: U256) -> TxEnv {
        TxEnv {
            caller,
            kind: TxKind::Call(to),
            value,
            gas_limit: 21_000,
            gas_price: 10,
            chain_id: Some(ARC_MAINNET_CHAIN_ID),
            ..Default::default()
        }
    }

    fn blocklist(evm: &mut ArcEvm<InMemoryDB, NoOpInspector>, address: Address) {
        evm.ctx_mut()
            .journal_mut()
            .load_account(NATIVE_COIN_CONTROL_ADDRESS)
            .unwrap();
        evm.ctx_mut()
            .journal_mut()
            .sstore(
                NATIVE_COIN_CONTROL_ADDRESS,
                blocklist_storage_slot(address),
                U256::ONE,
            )
            .unwrap();
    }

    #[test]
    fn transaction_sender_is_always_checked_and_receiver_only_for_nonzero_value() {
        let caller = Address::with_last_byte(1);
        let recipient = Address::with_last_byte(2);
        let flags = ArcChainConfig::mainnet().execution_spec_at(1, 1).arc_flags;

        let mut sender_blocked = evm_with_balance(caller, U256::from(1_000_000));
        blocklist(&mut sender_blocked, caller);
        sender_blocked
            .ctx_mut()
            .set_tx(value_call(caller, recipient, U256::ZERO));
        let err = ArcHandler::new(flags)
            .pre_execution(&mut sender_blocked)
            .unwrap_err();
        assert!(matches!(
            err,
            EVMError::Transaction(InvalidTransaction::Str(message))
                if message == ERR_BLOCKED_ADDRESS
        ));

        let mut zero_value = evm_with_balance(caller, U256::from(1_000_000));
        blocklist(&mut zero_value, recipient);
        zero_value
            .ctx_mut()
            .set_tx(value_call(caller, recipient, U256::ZERO));
        assert!(ArcHandler::new(flags)
            .pre_execution(&mut zero_value)
            .is_ok());

        let mut receiver_blocked = evm_with_balance(caller, U256::from(1_000_000));
        blocklist(&mut receiver_blocked, recipient);
        receiver_blocked
            .ctx_mut()
            .set_tx(value_call(caller, recipient, U256::ONE));
        assert!(matches!(
            ArcHandler::new(flags).pre_execution(&mut receiver_blocked),
            Err(EVMError::Transaction(InvalidTransaction::Str(message)))
                if message == ERR_BLOCKED_ADDRESS
        ));
    }

    #[test]
    fn blocklist_reads_are_unmetered_but_warm_the_slots() {
        let caller = Address::with_last_byte(1);
        let recipient = Address::with_last_byte(2);
        let flags = ArcChainConfig::mainnet().execution_spec_at(1, 1).arc_flags;
        let mut evm = evm_with_balance(caller, U256::from(1_000_000));
        evm.ctx_mut()
            .set_tx(value_call(caller, recipient, U256::ONE));

        let intrinsic = ArcHandler::new(flags)
            .validate_initial_tx_gas(&mut evm)
            .unwrap();
        assert_eq!(intrinsic.initial_gas, 21_000);
        ArcHandler::new(flags).pre_execution(&mut evm).unwrap();

        assert!(
            !evm.ctx_mut()
                .journal_mut()
                .sload(NATIVE_COIN_CONTROL_ADDRESS, blocklist_storage_slot(caller),)
                .unwrap()
                .is_cold
        );
        assert!(
            !evm.ctx_mut()
                .journal_mut()
                .sload(
                    NATIVE_COIN_CONTROL_ADDRESS,
                    blocklist_storage_slot(recipient),
                )
                .unwrap()
                .is_cold
        );
    }

    #[test]
    fn beneficiary_receives_base_fee_and_tip_without_transfer_log() {
        let caller = Address::with_last_byte(1);
        let beneficiary = Address::with_last_byte(9);
        let flags = ArcChainConfig::mainnet().execution_spec_at(1, 1).arc_flags;
        let mut evm = evm_with_balance(caller, U256::from(1_000_000));
        evm.ctx_mut().block.beneficiary = beneficiary;
        evm.ctx_mut()
            .set_tx(value_call(caller, Address::with_last_byte(2), U256::ZERO));
        let mut result = FrameResult::Call(CallOutcome::new(
            InterpreterResult::new(
                InstructionResult::Return,
                Default::default(),
                Gas::new_spent(21_000),
            ),
            0..0,
        ));

        ArcHandler::new(flags)
            .reward_beneficiary(&mut evm, &mut result)
            .unwrap();

        assert_eq!(
            evm.ctx_mut()
                .journal_mut()
                .load_account(beneficiary)
                .unwrap()
                .info
                .balance,
            U256::from(10 * 21_000)
        );
        assert!(evm.ctx().journaled_state.logs.is_empty());
    }
}
