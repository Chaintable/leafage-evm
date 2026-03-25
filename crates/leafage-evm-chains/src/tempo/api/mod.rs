use alloy_evm::precompiles::PrecompilesMap;
use std::ops::{Deref, DerefMut};

use crate::tempo::precompile::extend_tempo_precompiles;
use alloy_evm::{Database, EvmEnv};
use leafage_evm_types::MainnetSpecId;
use revm::{
    context::{BlockEnv, CfgEnv, Evm as EvmCtx, FrameStack, JournalTr, TxEnv},
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::EthInstructions,
        EthFrame, EvmTr, FrameInitOrResult, FrameResult,
    },
    inspector::InspectorEvmTr,
    interpreter::{interpreter::EthInterpreter, interpreter_action::FrameInit},
    precompile::{PrecompileSpecId, Precompiles},
    Context, Inspector, Journal,
};

mod exec;

/// Type alias for the default context type of the TempoEvm.
pub type TempoContext<DB> = Context<BlockEnv, TxEnv, CfgEnv<MainnetSpecId>, DB>;

/// Tempo EVM implementation.
///
/// This is a wrapper type around the `revm` evm with optional [`Inspector`] (tracing)
/// support. [`Inspector`] support is configurable at runtime because it's part of the underlying
/// EVM context.
///
/// Tempo uses standard Ethereum execution semantics (`MainnetHandler`) with custom
/// precompiles registered via [`extend_tempo_precompiles`].
#[allow(missing_debug_implementations)]
pub struct TempoEvm<DB: revm::database::Database, I> {
    pub inner: EvmCtx<
        TempoContext<DB>,
        I,
        EthInstructions<EthInterpreter, TempoContext<DB>>,
        PrecompilesMap,
        EthFrame,
    >,
    pub inspect: bool,
}

impl<DB: Database, I> TempoEvm<DB, I> {
    /// Creates a new [`TempoEvm`].
    ///
    /// This constructor:
    /// 1. Loads standard Ethereum precompiles for the given spec
    /// 2. Extends them with all 9 Tempo precompiles via [`extend_tempo_precompiles`]
    /// 3. Builds the EVM context with the merged precompile set
    pub fn new(env: EvmEnv<MainnetSpecId>, db: DB, inspector: I, inspect: bool) -> Self {
        let mut precompiles = PrecompilesMap::from_static(
            Precompiles::new(PrecompileSpecId::from_spec_id(env.cfg_env.spec)),
        );
        extend_tempo_precompiles(&mut precompiles, env.cfg_env.chain_id);

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
                instruction: EthInstructions::new_mainnet(),
                precompiles,
                frame_stack: Default::default(),
            },
            inspect,
        }
    }
}

impl<DB: Database, I> TempoEvm<DB, I> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &TempoContext<DB> {
        &self.inner.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut TempoContext<DB> {
        &mut self.inner.ctx
    }
}

impl<DB: Database, I> Deref for TempoEvm<DB, I> {
    type Target = TempoContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for TempoEvm<DB, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, INSP> EvmTr for TempoEvm<DB, INSP>
where
    DB: Database,
{
    type Context = TempoContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, TempoContext<DB>>;
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

impl<DB, INSP> InspectorEvmTr for TempoEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<TempoContext<DB>, EthInterpreter>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use revm::database::EmptyDB;
    use revm::inspector::NoOpInspector;

    #[test]
    fn test_tempo_evm_constructs() {
        let env = EvmEnv::new(
            CfgEnv::new_with_spec(MainnetSpecId::PRAGUE),
            BlockEnv::default(),
        );
        // Should not panic -- verifies precompile registration works
        let _evm = TempoEvm::new(env, EmptyDB::default(), NoOpInspector, false);
    }
}
