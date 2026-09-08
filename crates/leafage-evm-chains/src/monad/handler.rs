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
//!   a violation reverts the transaction with all gas consumed.

use crate::monad::page_tracker::{access_page, JournalPageStore};
use crate::monad::reserve_balance::dipped_into_reserve;
use crate::monad::{MonadContext, MonadEvm, MonadHaltReason};
use alloy_evm::Database;
use revm::bytecode::Bytecode;
use revm::context::result::{EVMError, ExecutionResult, ResultGas};
use revm::context::{
    Block, Cfg, ContextTr, JournalTr, LocalContextTr, Transaction, TransactionType,
};
use revm::context_interface::journaled_state::account::JournaledAccountTr;
use revm::context_interface::journaled_state::JournalCheckpoint;
use revm::context_interface::transaction::AccessListItemTr;
use revm::handler::{execution, post_execution, pre_execution, EthFrame, EvmTr, FrameTr, Handler};
use revm::inspector::InspectorHandler;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_action::FrameInit;
use revm::interpreter::{Gas, InstructionResult, SharedMemory};
use revm::primitives::{Bytes, U256};
use revm::Inspector;

pub struct MonadHandler<DB: revm::database::Database, INSP> {
    /// Journal position before the top level message, used to revert the
    /// transaction on a reserve balance violation.
    tx_checkpoint: Option<JournalCheckpoint>,
    /// The top level message was reverted by the reserve balance rule.
    reserve_balance_violation: bool,
    _phantom: core::marker::PhantomData<(MonadEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> MonadHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            tx_checkpoint: None,
            reserve_balance_violation: false,
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

    /// Stock revm frame input plus a journal checkpoint taken right before
    /// the top level message (after the EIP-7702 authorizations, like
    /// `reject_frame` at depth 0 in `execute_message.cpp`).
    fn first_frame_input(
        &mut self,
        evm: &mut Self::Evm,
        gas_limit: u64,
    ) -> Result<FrameInit, Self::Error> {
        let ctx = evm.ctx_mut();
        self.reserve_balance_violation = false;
        self.tx_checkpoint = if ctx.cfg().spec().is_reserve_balance_check_enabled() {
            let journal = ctx.journal_mut();
            // Record the journal position without changing the call depth.
            let checkpoint = journal.checkpoint();
            journal.checkpoint_commit();
            Some(checkpoint)
        } else {
            None
        };

        let mut memory = SharedMemory::new_with_buffer(ctx.local().shared_memory_buffer().clone());
        memory.set_memory_limit(ctx.cfg().memory_limit());

        let (tx, journal) = ctx.tx_journal_mut();
        let bytecode = if let Some(&to) = tx.kind().to() {
            let account = &journal.load_account_with_code(to)?.info;

            if let Some(delegated_address) =
                account.code.as_ref().and_then(Bytecode::eip7702_address)
            {
                let account = &journal.load_account_with_code(delegated_address)?.info;
                Some((
                    account.code.clone().unwrap_or_default(),
                    account.code_hash(),
                ))
            } else {
                Some((
                    account.code.clone().unwrap_or_default(),
                    account.code_hash(),
                ))
            }
        } else {
            None
        };

        Ok(FrameInit {
            depth: 0,
            memory,
            frame_input: execution::create_init_frame(tx, bytecode, gas_limit),
        })
    }

    /// `execute_message.cpp` depth 0: `revert_transaction` turns the result
    /// into `EVMC_MONAD_RESERVE_BALANCE_VIOLATION` (state rejected, gas
    /// consumed), then the stock gas accounting runs.
    fn last_frame_result(
        &mut self,
        evm: &mut Self::Evm,
        frame_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        if let Some(checkpoint) = self.tx_checkpoint.take() {
            if dipped_into_reserve(evm.ctx_mut()).map_err(EVMError::Database)? {
                evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
                let result = frame_result.interpreter_result_mut();
                result.result = InstructionResult::OutOfFunds;
                result.gas = Gas::new_spent(result.gas.limit());
                result.output = Bytes::new();
                self.reserve_balance_violation = true;
            }
        }

        let instruction_result = frame_result.interpreter_result().result;
        let gas = frame_result.gas_mut();
        let remaining = gas.remaining();
        let refunded = gas.refunded();

        *gas = Gas::new_spent(evm.ctx().tx().gas_limit());

        if instruction_result.is_ok_or_revert() {
            gas.erase_cost(remaining);
        }

        if instruction_result.is_ok() {
            gas.record_refund(refunded);
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
        if core::mem::take(&mut self.reserve_balance_violation) {
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
