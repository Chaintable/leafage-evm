use crate::tempo::api::{TempoContext, TempoEvm};
use crate::tempo::tx::{TempoCall, TempoTxEnv};
use alloy_evm::Database;
use revm::{
    context::{BlockEnv, ContextSetters},
    context_interface::{
        result::{EVMError, ExecutionResult, ResultAndState},
        ContextTr, JournalTr,
    },
    handler::{EthFrame, FrameResult, Handler, MainnetHandler},
    inspector::{InspectCommitEvm, InspectEvm, Inspector, InspectorHandler},
    interpreter::{interpreter::EthInterpreter, Gas, InitialAndFloorGas},
    state::EvmState,
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

/// Tempo handler — wraps [`MainnetHandler`] with batch execution support.
///
/// For standard (non-batch) transactions, delegates to [`MainnetHandler`].
/// For Tempo batch transactions (type 0x76 with `aa_calls`), executes each
/// call atomically using journal checkpoints.
pub struct TempoHandler<DB: revm::database::Database, INSP> {
    _phantom: core::marker::PhantomData<(TempoEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> TempoHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for TempoHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for TempoHandler<DB, INSP> {
    type Evm = TempoEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = revm::context::result::HaltReason;

    /// TIP-1000: Transactions with nonce == 0 require additional `new_account_cost` gas.
    /// Ported from Tempo writer: crates/revm/src/handler.rs
    #[inline]
    fn validate_initial_tx_gas(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<InitialAndFloorGas, Self::Error> {
        use revm::context_interface::cfg::gas_params::GasId;
        use revm::context_interface::Cfg;

        // Delegate to mainnet handler for base gas calculation.
        let mut init_gas = MainnetHandler::<Self::Evm, Self::Error, EthFrame>::default()
            .validate_initial_tx_gas(evm)?;

        // TIP-1000: nonce == 0 requires additional new_account_cost (250k gas).
        let hardfork =
            crate::tempo::hardfork::TempoHardfork::from_timestamp(
                evm.ctx().block.timestamp.saturating_to::<u64>(),
            );
        if hardfork.is_t1() && evm.ctx().tx.base.nonce == 0 {
            init_gas.initial_gas += evm.ctx().cfg.gas_params.get(GasId::new_account_cost());
        }

        Ok(init_gas)
    }

    /// Overridden execution: dispatches to batch path when `aa_calls` is present.
    #[inline]
    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        // Check whether this is a batch AA transaction.
        let calls = evm
            .ctx()
            .tx
            .tempo_fields
            .as_ref()
            .filter(|f| !f.aa_calls.is_empty())
            .map(|f| f.aa_calls.clone());

        if let Some(calls) = calls {
            self.execute_multi_call(evm, init_and_floor_gas, calls)
        } else {
            // Standard single call — delegate to mainnet handler.
            MainnetHandler::<Self::Evm, Self::Error, EthFrame>::default()
                .execution(evm, init_and_floor_gas)
        }
    }
}

impl<DB: Database, INSP> TempoHandler<DB, INSP> {
    /// Executes a batch of calls atomically.
    ///
    /// Uses journal checkpoints for atomicity: if any call reverts or halts,
    /// all state changes from the entire batch are rolled back.
    fn execute_multi_call(
        &mut self,
        evm: &mut TempoEvm<DB, INSP>,
        init_and_floor_gas: &InitialAndFloorGas,
        calls: Vec<TempoCall>,
    ) -> Result<FrameResult, EVMError<DB::Error>> {
        // Create a checkpoint so we can roll back on failure.
        let checkpoint = evm.ctx_mut().journal_mut().checkpoint();

        let gas_limit = evm.ctx().tx.base.gas_limit;
        let mut remaining_gas = gas_limit.saturating_sub(init_and_floor_gas.initial_gas);
        let mut accumulated_gas_refund: i64 = 0;

        // Save original TxEnv fields to restore after each call.
        let original_kind = evm.ctx().tx.base.kind;
        let original_value = evm.ctx().tx.base.value;
        let original_data = evm.ctx().tx.base.data.clone();

        let mut final_result: Option<FrameResult> = None;

        for call in &calls {
            // Patch TxEnv to point to the current call.
            {
                let tx = &mut evm.ctx_mut().tx;
                tx.base.kind = call.to;
                tx.base.value = call.value;
                tx.base.data = call.input.clone();
                tx.base.gas_limit = remaining_gas;
            }

            // Execute with zero initial gas (already deducted upfront).
            let zero_init = InitialAndFloorGas::new(0, 0);
            let result: Result<FrameResult, EVMError<DB::Error>> =
                MainnetHandler::default().execution(evm, &zero_init);

            // Restore original fields immediately, even on failure.
            {
                let tx = &mut evm.ctx_mut().tx;
                tx.base.kind = original_kind;
                tx.base.value = original_value;
                tx.base.data = original_data.clone();
                tx.base.gas_limit = gas_limit;
            }

            let mut frame_result = result?;

            if !frame_result.instruction_result().is_ok() {
                // Revert ALL state changes from the entire batch.
                evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);

                // Fix gas accounting: total gas spent = previous calls + failed call.
                let gas_spent_by_failed = frame_result.gas().spent();
                let total_gas_spent = (gas_limit - remaining_gas) + gas_spent_by_failed;

                let mut corrected_gas = Gas::new(gas_limit);
                if frame_result.instruction_result().is_revert() {
                    corrected_gas.set_spent(total_gas_spent);
                } else {
                    corrected_gas.spend_all();
                }
                corrected_gas.set_refund(0);
                *frame_result.gas_mut() = corrected_gas;

                return Ok(frame_result);
            }

            // Accumulate gas usage for successful calls.
            let gas_spent = frame_result.gas().spent();
            let gas_refunded = frame_result.gas().refunded();
            accumulated_gas_refund = accumulated_gas_refund.saturating_add(gas_refunded);
            remaining_gas = remaining_gas.saturating_sub(gas_spent);

            final_result = Some(frame_result);
        }

        // All calls succeeded — commit checkpoint to finalise state.
        evm.ctx_mut().journal_mut().checkpoint_commit();

        // Fix gas accounting for the entire batch.
        let mut result = final_result
            .ok_or_else(|| EVMError::Custom("No calls executed in batch".into()))?;

        let total_gas_spent = gas_limit - remaining_gas;
        let mut corrected_gas = Gas::new(gas_limit);
        corrected_gas.set_spent(total_gas_spent);
        corrected_gas.set_refund(accumulated_gas_refund);
        *result.gas_mut() = corrected_gas;

        Ok(result)
    }
}

impl<DB, INSP> InspectorHandler for TempoHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<TempoContext<DB>>,
{
    type IT = EthInterpreter;
}

impl<DB, INSP> ExecuteEvm for TempoEvm<DB, INSP>
where
    DB: Database,
{
    type ExecutionResult = ExecutionResult;
    type State = EvmState;
    type Error = EVMError<DB::Error>;
    type Tx = TempoTxEnv;
    type Block = BlockEnv;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        TempoHandler::new().run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.finalize()
    }

    fn replay(&mut self) -> Result<ResultAndState, Self::Error> {
        TempoHandler::new().run(self).map(|result| {
            let state = self.finalize();
            ResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP> ExecuteCommitEvm for TempoEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, INSP> InspectEvm for TempoEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<TempoContext<DB>>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.set_inspector(inspector);
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        TempoHandler::new().inspect_run(self)
    }
}

impl<DB, INSP> InspectCommitEvm for TempoEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<TempoContext<DB>>,
{
}
