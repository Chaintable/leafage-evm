use crate::arbitrum::context::ArbitrumExecutionContext;
use crate::arbitrum::hardforks::ArbitrumHardfork;
use crate::arbitrum::precompile::{ArbitrumContext, ArbitrumPrecompileEnv, ArbitrumPrecompiles};
use crate::arbitrum::tx::ArbitrumTxEnv;
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::context::{Context, ContextSetters, Evm, FrameStack, JournalTr};
use revm::handler::evm::{ContextDbError, FrameInitResult};
use revm::handler::instructions::EthInstructions;
use revm::handler::{
    EthFrame, EvmTr, ExecuteCommitEvm, ExecuteEvm, FrameInitOrResult, FrameResult, Handler,
    MainnetHandler,
};
use revm::inspector::{InspectCommitEvm, InspectEvm, Inspector, InspectorEvmTr, InspectorHandler};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_action::FrameInit;
use revm::interpreter::FrameInput;
use revm::state::EvmState;
use revm::{
    context_interface::{
        result::{EVMError, ExecutionResult, HaltReason, ResultAndState},
        ContextTr,
    },
    Database, DatabaseCommit, DatabaseRef, Journal,
};
use std::ops::{Deref, DerefMut};

pub struct ArbitrumEvm<DB: Database + DatabaseRef, I> {
    pub inner: Evm<
        ArbitrumContext<DB>,
        I,
        EthInstructions<EthInterpreter, ArbitrumContext<DB>>,
        ArbitrumPrecompiles,
        EthFrame,
    >,
}

impl<DB: Database + DatabaseRef, I> ArbitrumEvm<DB, I> {
    pub fn new(
        block_env: BlockEnv,
        cfg: CfgEnv<ArbitrumHardfork>,
        db: DB,
        inspector: I,
        precompile_env: ArbitrumPrecompileEnv,
    ) -> Self {
        let hardfork = cfg.spec;
        let spec = hardfork.into();
        Self {
            inner: Evm {
                ctx: Context {
                    block: block_env,
                    tx: ArbitrumTxEnv::default(),
                    cfg,
                    journaled_state: Journal::new(db),
                    chain: ArbitrumExecutionContext::default(),
                    local: Default::default(),
                    error: Ok(()),
                },
                inspector,
                instruction: EthInstructions::new_mainnet_with_spec(spec),
                precompiles: ArbitrumPrecompiles::new_with_env(hardfork, precompile_env),
                frame_stack: Default::default(),
            },
        }
    }

    pub const fn ctx(&self) -> &ArbitrumContext<DB> {
        &self.inner.ctx
    }

    pub fn ctx_mut(&mut self) -> &mut ArbitrumContext<DB> {
        &mut self.inner.ctx
    }

    fn note_frame_context(&mut self, frame_init: &FrameInit) {
        let callers_caller = self.current_frame_caller();
        self.inner
            .ctx
            .chain
            .set_current_call(frame_init.depth.saturating_add(1), callers_caller);
    }

    fn current_frame_caller(&mut self) -> alloy::primitives::Address {
        if self.inner.frame_stack.index().is_none() {
            return alloy::primitives::Address::ZERO;
        }

        match &self.inner.frame_stack.get().input {
            FrameInput::Call(inputs) => inputs.caller,
            FrameInput::Create(inputs) => inputs.caller(),
            FrameInput::Empty => alloy::primitives::Address::ZERO,
        }
    }
}

impl<DB: Database + DatabaseRef, I> Deref for ArbitrumEvm<DB, I> {
    type Target = ArbitrumContext<DB>;

    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database + DatabaseRef, I> DerefMut for ArbitrumEvm<DB, I> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, INSP> EvmTr for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseRef,
{
    type Context = ArbitrumContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, ArbitrumContext<DB>>;
    type Precompiles = ArbitrumPrecompiles;
    type Frame = EthFrame;

    fn all(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
    ) {
        self.inner.all()
    }

    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        self.inner.all_mut()
    }

    fn frame_init(
        &mut self,
        frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        self.note_frame_context(&frame_init);
        self.inner.frame_init(frame_init)
    }

    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        self.inner.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
        self.inner.frame_return_result(result)
    }
}

impl<DB, INSP> ExecuteEvm for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseRef,
{
    type ExecutionResult = ExecutionResult<HaltReason>;
    type State = EvmState;
    type Error = EVMError<<DB as Database>::Error>;
    type Tx = ArbitrumTxEnv;
    type Block = BlockEnv;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.ctx.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        MainnetHandler::default().run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.ctx.journal_mut().finalize()
    }

    fn replay(&mut self) -> Result<ResultAndState, Self::Error> {
        MainnetHandler::default().run(self).map(|result| {
            let state = self.finalize();
            ResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP> ExecuteCommitEvm for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseCommit + DatabaseRef,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, INSP> InspectEvm for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseRef,
    INSP: Inspector<ArbitrumContext<DB>, EthInterpreter>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.inspector = inspector;
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        MainnetHandler::default().inspect_run(self)
    }
}

impl<DB, INSP> InspectCommitEvm for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseCommit + DatabaseRef,
    INSP: Inspector<ArbitrumContext<DB>, EthInterpreter>,
{
}

impl<DB, INSP> InspectorEvmTr for ArbitrumEvm<DB, INSP>
where
    DB: Database + DatabaseRef,
    INSP: Inspector<ArbitrumContext<DB>, EthInterpreter>,
{
    type Inspector = INSP;

    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        self.inner.all_inspector()
    }

    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        self.inner.all_mut_inspector()
    }
}
