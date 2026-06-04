use alloy_evm::precompiles::PrecompilesMap;
use std::ops::{Deref, DerefMut};

use crate::bsc::hardforks::bsc::BscHardfork;
use crate::bsc::precompile::BscPrecompiles;
use crate::bsc::transaction::BscTxEnv;
use alloy_evm::{Database, EvmEnv};
use revm::{
    context::{BlockEnv, CfgEnv, Evm as EvmCtx, FrameStack, JournalTr},
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::EthInstructions,
        EthFrame, EvmTr, FrameInitOrResult, FrameResult,
    },
    inspector::InspectorEvmTr,
    interpreter::{interpreter::EthInterpreter, interpreter_action::FrameInit},
    Context, Inspector, Journal,
};
use revm::primitives::hardfork::SpecId;

mod exec;

/// Type alias for the default context type of the BscEvm.
pub type BscContext<DB> = Context<BlockEnv, BscTxEnv, CfgEnv<BscHardfork>, DB>;

/// BSC EVM implementation.
///
/// This is a wrapper type around the `revm` evm with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
#[allow(missing_debug_implementations)]
pub struct BscEvm<DB: revm::database::Database, I> {
    pub inner: EvmCtx<
        BscContext<DB>,
        I,
        EthInstructions<EthInterpreter, BscContext<DB>>,
        PrecompilesMap,
        EthFrame,
    >,
    pub inspect: bool,
}

impl<DB: Database, I> BscEvm<DB, I> {
    /// Creates a new [`BscEvm`].
    pub fn new(env: EvmEnv<BscHardfork>, db: DB, inspector: I, inspect: bool) -> Self {
        let precompiles =
            PrecompilesMap::from_static(BscPrecompiles::new(env.cfg_env.spec).precompiles());

        // Match the instruction table to the configured spec; `new_mainnet()` defaults to
        // the latest spec (Prague), which undercharges pre-Berlin SLOAD in early blocks.
        let spec_id = SpecId::from(env.cfg_env.spec);

        Self {
            inner: EvmCtx {
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
                instruction: EthInstructions::new_mainnet_with_spec(spec_id),
                precompiles,
                frame_stack: Default::default(),
            },
            inspect,
        }
    }
}

impl<DB: Database, I> BscEvm<DB, I> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &BscContext<DB> {
        &self.inner.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut BscContext<DB> {
        &mut self.inner.ctx
    }
}

impl<DB: Database, I> Deref for BscEvm<DB, I> {
    type Target = BscContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for BscEvm<DB, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, INSP> EvmTr for BscEvm<DB, INSP>
where
    DB: Database,
{
    type Context = BscContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, BscContext<DB>>;
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

impl<DB, INSP> InspectorEvmTr for BscEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<BscContext<DB>, EthInterpreter>,
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
