use crate::cosmos::api::{CosmosContext, CosmosEvm};
use alloy_evm::Database;
use revm::context::TxEnv;
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

impl<DB, INSP> ExecuteEvm for CosmosEvm<DB, INSP>
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
        self.inner.transact_one(tx)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.finalize()
    }

    fn replay(&mut self) -> Result<ResultAndState, Self::Error> {
        self.inner.replay()
    }
}

impl<DB, INSP> ExecuteCommitEvm for CosmosEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, INSP> InspectEvm for CosmosEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CosmosContext<DB>>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.set_inspector(inspector);
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.inspect_one_tx(tx)
    }
}

impl<DB, INSP> InspectCommitEvm for CosmosEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<CosmosContext<DB>>,
{
}
