use crate::tempo::api::{TempoContext, TempoEvm};
use alloy_evm::Database;
use revm::{
    context::{BlockEnv, ContextSetters, TxEnv},
    context_interface::{
        result::{EVMError, ExecutionResult, ResultAndState},
        ContextTr,
    },
    handler::{EthFrame, Handler},
    inspector::{InspectCommitEvm, InspectEvm, Inspector, InspectorHandler},
    interpreter::interpreter::EthInterpreter,
    state::EvmState,
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

/// Tempo handler — wraps [`MainnetHandler`] with no custom overrides.
///
/// Tempo uses standard Ethereum execution semantics. Fee handling is skipped
/// (consistent with leafage's `disable_base_fee` behaviour).
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
    type Tx = TxEnv;
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
