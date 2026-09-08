use super::MonadEvm;
use crate::monad::api::MonadContext;
use crate::monad::handler::MonadHandler;
use alloy_evm::Database;
use revm::handler::Handler;
use revm::inspector::InspectorHandler;
use revm::{
    context::{BlockEnv, ContextSetters, TxEnv},
    context_interface::{
        result::{EVMError, ExecutionResult, ResultAndState},
        ContextTr,
    },
    inspector::{InspectCommitEvm, InspectEvm, Inspector},
    state::EvmState,
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

impl<DB, INSP> ExecuteEvm for MonadEvm<DB, INSP>
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
        MonadHandler::default().run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.finalize()
    }

    fn replay(&mut self) -> Result<ResultAndState, Self::Error> {
        MonadHandler::default().run(self).map(|result| {
            let state = self.finalize();
            ResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP> ExecuteCommitEvm for MonadEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, INSP> InspectEvm for MonadEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<MonadContext<DB>>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.set_inspector(inspector);
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        MonadHandler::default().inspect_run(self)
    }
}

impl<DB, INSP> InspectCommitEvm for MonadEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<MonadContext<DB>>,
{
}
