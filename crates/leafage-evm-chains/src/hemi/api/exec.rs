use crate::hemi::api::HemiEvm;
use crate::hemi::handler::HemiHandler;
use alloy_evm::Database;
use op_revm::{OpHaltReason, OpTransaction};
use revm::context::{ContextSetters, TxEnv};
use revm::handler::{EvmTr, Handler};
use revm::inspector::InspectorHandler;
use revm::{
    context::BlockEnv,
    context_interface::result::{EVMError, ExecutionResult, ResultAndState},
    inspector::{InspectCommitEvm, InspectEvm, Inspector},
    state::EvmState,
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

impl<DB, INSP> ExecuteEvm for HemiEvm<DB, INSP>
where
    DB: Database,
{
    type ExecutionResult = ExecutionResult<OpHaltReason>;
    type State = EvmState;
    type Error = EVMError<DB::Error>;
    type Tx = OpTransaction<TxEnv>;
    type Block = BlockEnv;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        let (ctx, _, _, _) = self.inner.all_mut();
        ctx.set_tx(tx);
        HemiHandler::default().run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.finalize()
    }

    fn replay(&mut self) -> Result<ResultAndState<Self::ExecutionResult>, Self::Error> {
        HemiHandler::default().run(self).map(|result| {
            let state = self.finalize();
            ResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP> ExecuteCommitEvm for HemiEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        let (ctx, _, _, _) = self.inner.all_mut();
        ctx.db_mut().commit(state);
    }
}

impl<DB, INSP> InspectEvm for HemiEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<super::HemiOpContext<DB>>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.set_inspector(inspector);
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        let (ctx, _, _, _) = self.inner.all_mut();
        ctx.set_tx(tx);
        HemiHandler::default().inspect_run(self)
    }
}

impl<DB, INSP> InspectCommitEvm for HemiEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<super::HemiOpContext<DB>>,
{
}
