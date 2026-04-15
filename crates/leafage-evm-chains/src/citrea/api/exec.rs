use crate::citrea::api::{CitreaContext, CitreaEvm};
use crate::citrea::handler::CitreaHandler;
use crate::citrea::l1_fee::calc_diff_size;
use alloy_evm::Database;
use revm::context::{ContextSetters, TxEnv};
use revm::handler::Handler;
use revm::inspector::InspectorHandler;
use revm::{
    context::BlockEnv,
    context_interface::{
        result::{EVMError, ExecutionResult, ResultAndState},
        ContextTr,
    },
    inspector::{InspectCommitEvm, InspectEvm, Inspector},
    state::EvmState,
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

impl<DB, INSP> ExecuteEvm for CitreaEvm<DB, INSP>
where
    DB: Database,
{
    type ExecutionResult = ExecutionResult;
    type State = EvmState;
    type Error = EVMError<DB::Error>;
    type Tx = TxEnv;
    type Block = BlockEnv;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        CitreaHandler::default().run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.finalize()
    }

    fn replay(&mut self) -> Result<ResultAndState, Self::Error> {
        CitreaHandler::default().run(self).map(|result| {
            let state = self.finalize();
            ResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP> ExecuteCommitEvm for CitreaEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, INSP> CitreaEvm<DB, INSP>
where
    DB: Database,
{
    /// Run a transaction and return `(ExecutionResult, diff_size)`.
    ///
    /// `diff_size` is computed from the journal *before* `execution_result`
    /// commits the transaction (which clears journal entries).
    pub fn transact_with_diff_size(
        &mut self,
        tx: TxEnv,
    ) -> Result<(ExecutionResult, usize), EVMError<DB::Error>> {
        self.inner.ctx.set_tx(tx);
        let mut handler = CitreaHandler::default();

        let result = self.run_with_diff_size(&mut handler);
        match result {
            ok @ Ok(_) => ok,
            Err(e) => handler.catch_error(self, e).map(|r| (r, 0)),
        }
    }

    /// Decomposed handler pipeline that captures diff_size between
    /// `post_execution` and `execution_result` (before journal commit).
    fn run_with_diff_size(
        &mut self,
        handler: &mut CitreaHandler<DB, INSP>,
    ) -> Result<(ExecutionResult, usize), EVMError<DB::Error>> {
        let init_and_floor_gas = handler.validate(self)?;
        let eip7702_refund = handler.pre_execution(self)? as i64;
        let mut exec_result = handler.execution(self, &init_and_floor_gas)?;
        let result_gas =
            handler.post_execution(self, &mut exec_result, init_and_floor_gas, eip7702_refund)?;

        // Capture diff_size while journal entries are still intact
        let diff_size = calc_diff_size(&mut self.inner.ctx);

        let result = handler.execution_result(self, exec_result, result_gas)?;
        Ok((result, diff_size))
    }
}

impl<DB, INSP> InspectEvm for CitreaEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CitreaContext<DB>>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.set_inspector(inspector);
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        CitreaHandler::default().inspect_run(self)
    }
}

impl<DB, INSP> InspectCommitEvm for CitreaEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<CitreaContext<DB>>,
{
}
