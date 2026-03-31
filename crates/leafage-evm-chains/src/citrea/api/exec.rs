use crate::citrea::api::{CitreaContext, CitreaEvm};
use crate::citrea::handler::CitreaHandler;
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
