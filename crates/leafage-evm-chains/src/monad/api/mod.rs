use crate::monad::evm::instructions::monad_instructions;
use crate::monad::precompile::MonadPrecompiles;
use crate::monad::MonadHardfork;
use alloy_evm::{Database, EvmEnv};
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::context::{Context, FrameStack};
use revm::context::{Evm, JournalTr, TxEnv};
use revm::handler::evm::{ContextDbError, FrameInitResult};
use revm::handler::instructions::EthInstructions;
use revm::handler::{EthFrame, EvmTr, FrameInitOrResult, FrameResult};
use revm::inspector::InspectorEvmTr;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_action::FrameInit;
use revm::{Inspector, Journal};
use std::ops::{Deref, DerefMut};

mod exec;

pub type MonadContext<DB> = Context<BlockEnv, TxEnv, CfgEnv<MonadHardfork>, DB>;

pub struct MonadEvm<DB: revm::database::Database, I> {
    pub inner: Evm<
        MonadContext<DB>,
        I,
        EthInstructions<EthInterpreter, MonadContext<DB>>,
        MonadPrecompiles,
        EthFrame,
    >,
}

impl<DB: Database, I> MonadEvm<DB, I> {
    pub fn new(env: EvmEnv<MonadHardfork>, db: DB, inspector: I) -> Self {
        let hardfork = env.cfg_env.spec;

        Self {
            inner: Evm {
                ctx: Context {
                    block: env.block_env,
                    cfg: env.cfg_env,
                    journaled_state: Journal::new(db),
                    tx: Default::default(),
                    chain: Default::default(),
                    local: Default::default(),
                    error: Ok(()),
                },
                inspector,
                instruction: monad_instructions::<DB>(hardfork),
                precompiles: MonadPrecompiles::new(hardfork),
                frame_stack: Default::default(),
            },
        }
    }
}

impl<DB: Database, I> MonadEvm<DB, I> {
    pub const fn ctx(&self) -> &MonadContext<DB> {
        &self.inner.ctx
    }

    pub fn ctx_mut(&mut self) -> &mut MonadContext<DB> {
        &mut self.inner.ctx
    }
}

impl<DB: Database, I> Deref for MonadEvm<DB, I> {
    type Target = MonadContext<DB>;

    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for MonadEvm<DB, I> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, INSP> EvmTr for MonadEvm<DB, INSP>
where
    DB: Database,
{
    type Context = MonadContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, MonadContext<DB>>;
    type Precompiles = MonadPrecompiles;
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
        frame_input: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        self.inner.frame_init(frame_input)
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

impl<DB, INSP> InspectorEvmTr for MonadEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<MonadContext<DB>, EthInterpreter>,
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
