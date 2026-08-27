use crate::polygon::instructions::polygon_instructions;
use crate::polygon::precompile::polygon_precompiles;
use crate::polygon::PolygonHardfork;
use alloy_evm::precompiles::PrecompilesMap;
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

pub type PolygonContext<DB> = Context<BlockEnv, TxEnv, CfgEnv<PolygonHardfork>, DB>;

pub struct PolygonEvm<DB: revm::database::Database, I> {
    pub inner: Evm<
        PolygonContext<DB>,
        I,
        EthInstructions<EthInterpreter, PolygonContext<DB>>,
        PrecompilesMap,
        EthFrame,
    >,
}

impl<DB: Database, I> PolygonEvm<DB, I> {
    pub fn new(env: EvmEnv<PolygonHardfork>, db: DB, inspector: I) -> Self {
        let hardfork = env.cfg_env.spec;
        let precompiles = PrecompilesMap::from_static(polygon_precompiles(hardfork));

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
                instruction: polygon_instructions::<DB>(hardfork),
                precompiles,
                frame_stack: Default::default(),
            },
        }
    }
}

impl<DB: Database, I> PolygonEvm<DB, I> {
    pub const fn ctx(&self) -> &PolygonContext<DB> {
        &self.inner.ctx
    }

    pub fn ctx_mut(&mut self) -> &mut PolygonContext<DB> {
        &mut self.inner.ctx
    }
}

impl<DB: Database, I> Deref for PolygonEvm<DB, I> {
    type Target = PolygonContext<DB>;

    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for PolygonEvm<DB, I> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, INSP> EvmTr for PolygonEvm<DB, INSP>
where
    DB: Database,
{
    type Context = PolygonContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, PolygonContext<DB>>;
    type Precompiles = PrecompilesMap;
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

impl<DB, INSP> InspectorEvmTr for PolygonEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<PolygonContext<DB>, EthInterpreter>,
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
