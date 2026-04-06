use crate::cosmos::config::CosmosEvmConfig;
use crate::cosmos::{CosmosHardfork, CosmosPrecompiles};
use alloy_evm::precompiles::PrecompilesMap;
use alloy_evm::{Database, EvmEnv};
use leafage_evm_types::{Address, BlockEnv, CfgEnv};
use revm::context::{Context, ContextError, FrameStack};
use revm::context::{Evm, JournalTr, TxEnv};
use revm::handler::evm::{ContextDbError, FrameInitResult};
use revm::handler::instructions::EthInstructions;
use revm::handler::{EthFrame, EvmTr, FrameInitOrResult, FrameResult, FrameTr};
use revm::inspector::InspectorEvmTr;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_action::FrameInit;
use revm::interpreter::FrameInput;
use revm::{Inspector, Journal};
use std::ops::{Deref, DerefMut};
use revm::primitives::hardfork::SpecId;

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
    pub config: CosmosEvmConfig,
}

impl<DB: Database, I> CosmosEvm<DB, I> {
    /// Creates a new [`CosmosEvm`].
    pub fn new(
        env: EvmEnv<CosmosHardfork>,
        evm_config: CosmosEvmConfig,
        db: DB,
        inspector: I,
        inspect: bool,
    ) -> Self {
        let cosmos_precompiles = CosmosPrecompiles::new(env.cfg_env.spec);
        let precompiles = PrecompilesMap::from_static(cosmos_precompiles.precompiles());
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
                instruction: EthInstructions::new_mainnet_with_spec(SpecId::default()),
                precompiles,
                frame_stack: Default::default(),
            },
            inspect,
            config: evm_config,
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
        self.check_unsupported_precompiles(&frame_input.frame_input)?;
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

impl<DB, INSP> InspectorEvmTr for CosmosEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CosmosContext<DB>, EthInterpreter>,
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

    fn inspect_frame_init(
        &mut self,
        frame_init: <Self::Frame as FrameTr>::FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        self.check_unsupported_precompiles(&frame_init.frame_input)?;
        self.inner.inspect_frame_init(frame_init)
    }
}

impl<DB, INSP> CosmosEvm<DB, INSP>
where
    DB: Database,
{
    fn check_unsupported_precompiles<D>(
        &self,
        frame_input: &FrameInput,
    ) -> Result<(), ContextError<D>> {
        let unsupported_precompiles = |addr: Address| {
            return Err(ContextError::Custom(format!(
                "unsupported precompile address: {}",
                addr
            )));
        };
        if let FrameInput::Call(ref call) = frame_input {
            if super::precompile::unsupported::is_unsupported(&call.bytecode_address) {
                return unsupported_precompiles(call.bytecode_address);
            }
            if let Some(addr) = self.config.native_token {
                if addr.eq(&call.bytecode_address) {
                    return unsupported_precompiles(call.bytecode_address);
                }
            }
        }
        Ok(())
    }
}
