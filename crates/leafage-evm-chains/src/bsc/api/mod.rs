use alloy_evm::precompiles::PrecompilesMap;
use std::ops::{Deref, DerefMut};

use crate::bsc::cas20;
use crate::bsc::hardforks::bsc::BscHardfork;
use crate::bsc::precompile::{milli_timestamp, BscPrecompiles};
use crate::bsc::transaction::BscTxEnv;
use alloy_evm::{Database, EvmEnv};
use revm::primitives::hardfork::SpecId;
use revm::{
    context::{BlockEnv, CfgEnv, ContextError, Evm as EvmCtx, FrameStack, JournalTr},
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::EthInstructions,
        EthFrame, EvmTr, FrameInitOrResult, FrameResult,
    },
    inspector::InspectorEvmTr,
    interpreter::{interpreter::EthInterpreter, interpreter_action::FrameInit, FrameInput},
    precompile::u64_to_address,
    primitives::Address,
    Context, Inspector, Journal,
};

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
        let mut precompiles =
            PrecompilesMap::from_static(BscPrecompiles::new(env.cfg_env.spec).precompiles());
        if env.cfg_env.spec >= BscHardfork::Jenner {
            install_jenner_precompiles(&mut precompiles);
        }

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

/// Jenner's context-aware precompiles: `0x70` reads the block's millisecond timestamp, and
/// CAS20 tokens are a whole address range, resolved by prefix rather than listed (go-bsc
/// `EVM.precompile`). Lookup-resolved addresses stay out of the warm set, as in go-bsc.
fn install_jenner_precompiles(precompiles: &mut PrecompilesMap) {
    precompiles.apply_precompile(
        &u64_to_address(milli_timestamp::MILLI_TIMESTAMP_ADDRESS),
        |_| Some(milli_timestamp::milli_timestamp()),
    );
    precompiles.set_precompile_lookup(|address: &Address| {
        cas20::is_cas20_token(address).then(cas20::token_precompile)
    });
}

/// Short-circuits a frame whose `bytecode_address` is a CAS20 singleton leafage forwards
/// (factory, activation registry, policy registry; see [`cas20::is_forwarded`]). The message
/// format is parsed by `leafage-evm-rpc` `ToJsonRpcError for EVMError` into `-39008`.
fn check_forwarded_precompiles<D>(
    spec: BscHardfork,
    frame_input: &FrameInput,
) -> Result<(), ContextError<D>> {
    if spec < BscHardfork::Jenner {
        return Ok(());
    }
    if let FrameInput::Call(call) = frame_input {
        if cas20::is_forwarded(&call.bytecode_address) {
            return Err(ContextError::Custom(format!(
                "unsupported precompile address: {}",
                call.bytecode_address
            )));
        }
    }
    Ok(())
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
        check_forwarded_precompiles(self.inner.ctx.cfg.spec, &frame_input.frame_input)?;
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

    fn inspect_frame_init(
        &mut self,
        frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        check_forwarded_precompiles(self.inner.ctx.cfg.spec, &frame_init.frame_input)?;
        self.inner.inspect_frame_init(frame_init)
    }
}
