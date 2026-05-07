use crate::iotex::IotexHardfork;
use alloy_evm::{Database, EvmEnv};
use leafage_evm_types::{Address, BlockEnv, CfgEnv};
use revm::context::{Context, ContextError, FrameStack};
use revm::context::{Evm, JournalTr, TxEnv};
use revm::handler::evm::{ContextDbError, FrameInitResult};
use revm::handler::instructions::EthInstructions;
use revm::handler::{EthFrame, EthPrecompiles, EvmTr, FrameInitOrResult, FrameResult, FrameTr};
use revm::inspector::InspectorEvmTr;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_action::FrameInit;
use revm::interpreter::FrameInput;
use revm::primitives::hardfork::SpecId;
use revm::{Inspector, Journal};
use std::ops::{Deref, DerefMut};

mod exec;

pub type IotexContext<DB> = Context<BlockEnv, TxEnv, CfgEnv<IotexHardfork>, DB>;

pub struct IotexEvm<DB: revm::database::Database, I> {
    pub inner: Evm<
        IotexContext<DB>,
        I,
        EthInstructions<EthInterpreter, IotexContext<DB>>,
        EthPrecompiles,
        EthFrame,
    >,
}

impl<DB: Database, I> IotexEvm<DB, I> {
    /// Creates a new [`IotexEvm`].
    pub fn new(env: EvmEnv<IotexHardfork>, db: DB, inspector: I) -> Self {
        let spec: SpecId = (*env.cfg_env.spec).into();
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
                instruction: EthInstructions::new_mainnet_with_spec(spec),
                precompiles: EthPrecompiles::new(spec),
                frame_stack: Default::default(),
            },
        }
    }
}

impl<DB: Database, I> IotexEvm<DB, I> {
    pub const fn ctx(&self) -> &IotexContext<DB> {
        &self.inner.ctx
    }

    pub fn ctx_mut(&mut self) -> &mut IotexContext<DB> {
        &mut self.inner.ctx
    }
}

impl<DB: Database, I> Deref for IotexEvm<DB, I> {
    type Target = IotexContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for IotexEvm<DB, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB, INSP> EvmTr for IotexEvm<DB, INSP>
where
    DB: Database,
{
    type Context = IotexContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, IotexContext<DB>>;
    type Precompiles = EthPrecompiles;
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

impl<DB, INSP> InspectorEvmTr for IotexEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<IotexContext<DB>, EthInterpreter>,
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

impl<DB, INSP> IotexEvm<DB, INSP>
where
    DB: Database,
{
    fn check_unsupported_precompiles<D>(
        &self,
        frame_input: &FrameInput,
    ) -> Result<(), ContextError<D>> {
        if let FrameInput::Call(ref call) = frame_input {
            if super::precompile::is_unsupported(&call.bytecode_address) {
                return Err(ContextError::Custom(format!(
                    "unsupported precompile address: {}",
                    Address::from(call.bytecode_address)
                )));
            }
        }
        Ok(())
    }
}
