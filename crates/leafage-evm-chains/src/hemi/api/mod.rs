use alloy_evm::Database;
use leafage_evm_types::{BlockEnv, CfgEnv};
use op_revm::{L1BlockInfo, OpEvm, OpTransaction};
use revm::context::{Context, ContextError, FrameStack, TxEnv};
use revm::handler::evm::{ContextDbError, FrameInitResult};
use revm::handler::instructions::EthInstructions;
use revm::handler::{EvmTr, FrameInitOrResult, FrameResult, FrameTr};
use revm::inspector::InspectorEvmTr;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_action::FrameInit;
use revm::interpreter::FrameInput;
use revm::Inspector;

mod exec;

pub type HemiOpContext<DB> = Context<
    BlockEnv,
    OpTransaction<TxEnv>,
    CfgEnv<op_revm::OpSpecId>,
    DB,
    revm::Journal<DB>,
    L1BlockInfo,
>;

pub type HemiInnerEvm<DB, INSP> =
    OpEvm<HemiOpContext<DB>, INSP, EthInstructions<EthInterpreter, HemiOpContext<DB>>>;

pub struct HemiEvm<DB: revm::database::Database, INSP> {
    pub inner: HemiInnerEvm<DB, INSP>,
    pub inspect: bool,
}

impl<DB: Database, INSP> HemiEvm<DB, INSP> {
    pub fn new(inner: HemiInnerEvm<DB, INSP>, inspect: bool) -> Self {
        Self { inner, inspect }
    }
}

impl<DB, INSP> EvmTr for HemiEvm<DB, INSP>
where
    DB: Database,
{
    type Context = <HemiInnerEvm<DB, INSP> as EvmTr>::Context;
    type Instructions = <HemiInnerEvm<DB, INSP> as EvmTr>::Instructions;
    type Precompiles = <HemiInnerEvm<DB, INSP> as EvmTr>::Precompiles;
    type Frame = <HemiInnerEvm<DB, INSP> as EvmTr>::Frame;

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

impl<DB, INSP> InspectorEvmTr for HemiEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<HemiOpContext<DB>, EthInterpreter>,
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

impl<DB, INSP> HemiEvm<DB, INSP>
where
    DB: Database,
{
    fn check_unsupported_precompiles<D>(
        &self,
        frame_input: &FrameInput,
    ) -> Result<(), ContextError<D>> {
        if let FrameInput::Call(ref call) = frame_input {
            if super::precompile::unsupported::is_unsupported(&call.bytecode_address) {
                return Err(ContextError::Custom(format!(
                    "unsupported precompile address: {}",
                    call.bytecode_address
                )));
            }
        }
        Ok(())
    }
}
