//! Transaction level deviations.
//!
//! * `monad_transaction_gas.cpp` `compute_gas_refund` returns zero from
//!   MONAD_ONE: there are no EIP-3529 refunds *and* unused gas is not
//!   returned to the sender, the sender pays `gas_limit * gas_price` and the
//!   receipt reports `gas_used == gas_limit`
//!   (`execute_transaction.cpp`). The [`ExecutionResult`] returned here keeps
//!   the gas actually spent by the execution so gas estimation can search
//!   for the smallest working gas limit; the settlement (no reimbursement,
//!   beneficiary paid on the full gas limit) follows the node.
//! * `execute_transaction.cpp` with MIP-8: the EIP-2930 access list warms
//!   storage *pages*, not slots.
//! * `execute_message.cpp`: at depth 0 the reserve balance rule is checked and
//!   a violation rejects the top level message with all gas consumed
//!   ([`MonadEvm`] frame hooks, reported here as
//!   [`MonadHaltReason::ReserveBalanceViolation`]).

use crate::monad::page_tracker::{access_page, JournalPageStore};
use crate::monad::{MonadContext, MonadEvm, MonadHaltReason};
use alloy_evm::Database;
use revm::context::result::{EVMError, ExecutionResult, ResultGas};
use revm::context::{Block, ContextTr, JournalTr, LocalContextTr, Transaction, TransactionType};
use revm::context_interface::journaled_state::account::JournaledAccountTr;
use revm::context_interface::transaction::AccessListItemTr;
use revm::handler::{post_execution, pre_execution, EthFrame, EvmTr, FrameTr, Handler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::primitives::U256;
use revm::Inspector;

pub struct MonadHandler<DB: revm::database::Database, INSP> {
    _phantom: core::marker::PhantomData<(MonadEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> MonadHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for MonadHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for MonadHandler<DB, INSP> {
    type Evm = MonadEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = MonadHaltReason;

    fn load_accounts(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        evm.reserve_balance_violation = false;
        pre_execution::load_accounts::<_, Self::Error>(evm)?;
        let ctx = evm.ctx_mut();
        if !ctx.cfg().spec().is_mip8_enabled() {
            return Ok(());
        }
        let (tx, journal) = ctx.tx_journal_mut();
        if tx.tx_type() == TransactionType::Legacy {
            return Ok(());
        }
        if let Some(access_list) = tx.access_list() {
            let mut store = JournalPageStore(journal);
            for item in access_list {
                let address = *item.address();
                for slot in item.storage_slots() {
                    access_page(&mut store, address, U256::from_be_bytes(slot.0));
                }
            }
        }
        Ok(())
    }

    /// `compute_gas_refund == 0`: no EIP-3529 refunds.
    fn refund(
        &self,
        _evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        _eip7702_refund: i64,
    ) {
        exec_result.gas_mut().set_refund(0);
    }

    /// `compute_gas_refund == 0`: unused gas is not returned to the sender.
    fn reimburse_caller(
        &self,
        _evm: &mut Self::Evm,
        _exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    /// `calculate_txn_award(tx, base_fee, gas_used = gas_limit)`: the
    /// beneficiary receives the priority fee on the whole gas limit.
    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        _exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let ctx = evm.ctx_mut();
        let basefee = ctx.block().basefee() as u128;
        let beneficiary = ctx.block().beneficiary();
        let tx = ctx.tx();
        let priority_fee = tx.effective_gas_price(basefee).saturating_sub(basefee);
        let reward = U256::from(priority_fee.saturating_mul(tx.gas_limit() as u128));
        ctx.journal_mut()
            .load_account_mut(beneficiary)
            .map_err(EVMError::Database)?
            .incr_balance(reward);
        Ok(())
    }

    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        result_gas: ResultGas,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        if let Err(error) = core::mem::replace(evm.ctx_mut().error(), Ok(())) {
            return Err(error.into());
        }

        let mut exec_result =
            post_execution::output::<_, MonadHaltReason>(evm.ctx_mut(), result, result_gas);
        if core::mem::take(&mut evm.reserve_balance_violation) {
            if let ExecutionResult::Halt { reason, .. } = &mut exec_result {
                *reason = MonadHaltReason::ReserveBalanceViolation;
            }
        }

        evm.ctx_mut().journal_mut().commit_tx();
        evm.ctx_mut().local_mut().clear();
        evm.frame_stack().clear();

        Ok(exec_result)
    }
}

impl<DB, INSP> InspectorHandler for MonadHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<MonadContext<DB>>,
{
    type IT = EthInterpreter;
}
