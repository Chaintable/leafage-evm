use crate::cosmos::{CosmosHardfork, CosmosPrecompiles};
use alloy_evm::precompiles::PrecompilesMap;
use alloy_evm::{Database, EvmEnv};
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::context::{Context, ContextError, FrameStack};
use revm::context::{Evm, JournalTr, TxEnv};
use revm::handler::evm::{ContextDbError, FrameInitResult};
use revm::handler::instructions::EthInstructions;
use revm::handler::{EthFrame, EvmTr, FrameInitOrResult, FrameResult};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_action::FrameInit;
use revm::interpreter::FrameInput;
use revm::Journal;
use std::ops::{Deref, DerefMut};

mod exec;

pub type CosmosContext<DB> = Context<BlockEnv, TxEnv, CfgEnv<CosmosHardfork>, DB>;

pub struct CosmosEvm<DB: revm::database::Database, I> {
    pub inner: Evm<
        CosmosContext<DB>,
        I,
        EthInstructions<EthInterpreter, CosmosContext<DB>>,
        PrecompilesMap,
        EthFrame,
    >,
    pub inspect: bool,
}

impl<DB: Database, I> CosmosEvm<DB, I> {
    /// Creates a new [`CosmosEvm`].
    pub fn new(env: EvmEnv<CosmosHardfork>, db: DB, inspector: I, inspect: bool) -> Self {
        let precompiles =
            PrecompilesMap::from_static(CosmosPrecompiles::new(env.cfg_env.spec).precompiles());

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
                instruction: EthInstructions::new_mainnet(),
                precompiles,
                frame_stack: Default::default(),
            },
            inspect,
        }
    }
}

impl<DB: Database, I> CosmosEvm<DB, I> {
    /// Provides a reference to the EVM context.
    pub const fn ctx(&self) -> &CosmosContext<DB> {
        &self.inner.ctx
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut CosmosContext<DB> {
        &mut self.inner.ctx
    }
}

impl<DB: Database, I> Deref for CosmosEvm<DB, I> {
    type Target = CosmosContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for CosmosEvm<DB, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, INSP> EvmTr for CosmosEvm<DB, INSP>
where
    DB: Database,
{
    type Context = CosmosContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, CosmosContext<DB>>;
    type Precompiles = PrecompilesMap;
    type Frame = EthFrame;

    fn ctx(&mut self) -> &mut Self::Context {
        self.inner.ctx_mut()
    }

    fn ctx_ref(&self) -> &Self::Context {
        self.inner.ctx_ref()
    }

    fn ctx_instructions(&mut self) -> (&mut Self::Context, &mut Self::Instructions) {
        self.inner.ctx_instructions()
    }

    fn ctx_precompiles(&mut self) -> (&mut Self::Context, &mut Self::Precompiles) {
        self.inner.ctx_precompiles()
    }

    /// Returns a mutable reference to the frame stack.
    fn frame_stack(&mut self) -> &mut FrameStack<Self::Frame> {
        self.inner.frame_stack()
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
        let frame = self.inner.frame_stack().get();
        if let FrameInput::Call(ref call) = frame.input {
            if super::precompile::unsupported::is_unsupported(&call.target_address) {
                return Err(ContextError::Custom(format!(
                    "unsupported precompile address: {:?}",
                    call.target_address
                )));
            }
        }
        self.inner.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
        self.inner.frame_return_result(result)
    }
}
