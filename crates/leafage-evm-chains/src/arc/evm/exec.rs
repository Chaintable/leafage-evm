use super::{ArcContext, ArcEvm};
use crate::arc::handler::ArcHandler;
use alloy_evm::Database;
use leafage_evm_types::BlockEnv;
use revm::{
    context::{
        result::{EVMError, ExecutionResult, ResultAndState},
        ContextSetters, ContextTr, TxEnv,
    },
    handler::Handler,
    inspector::{InspectCommitEvm, InspectEvm, Inspector, InspectorHandler},
    state::EvmState,
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

impl<DB: Database, I> ExecuteEvm for ArcEvm<DB, I> {
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
        ArcHandler::new(self.execution_spec().arc_flags).run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.finalize()
    }

    fn replay(&mut self) -> Result<ResultAndState, Self::Error> {
        ArcHandler::new(self.execution_spec().arc_flags)
            .run(self)
            .map(|result| ResultAndState::new(result, self.finalize()))
    }
}

impl<DB, I> ExecuteCommitEvm for ArcEvm<DB, I>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, I> InspectEvm for ArcEvm<DB, I>
where
    DB: Database,
    I: Inspector<ArcContext<DB>>,
{
    type Inspector = I;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.set_inspector(inspector);
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        ArcHandler::new(self.execution_spec().arc_flags).inspect_run(self)
    }
}

impl<DB, I> InspectCommitEvm for ArcEvm<DB, I>
where
    DB: Database + DatabaseCommit,
    I: Inspector<ArcContext<DB>>,
{
}
