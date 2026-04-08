use std::ops::{Deref, DerefMut};

use alloy_evm::precompiles::PrecompilesMap;
use alloy_evm::Database;
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId, U256};
use revm::context::{Evm as EvmCtx, FrameStack};
use revm::context_interface::JournalTr;
use revm::handler::evm::{ContextDbError, FrameInitResult};
use revm::handler::instructions::EthInstructions;
use revm::handler::{EthFrame, EvmTr, FrameInitOrResult, FrameResult};
use revm::inspector::InspectorEvmTr;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_action::FrameInit;
use revm::primitives::hardfork::SpecId;
use revm::{Context, Inspector, Journal};

use crate::citrea::precompile::CitreaPrecompiles;
use crate::citrea::CitreaHardfork;

mod exec;

// ── Chain extension context ─────────────────────────────────────────

/// Holds per-transaction L1 fee info collected during handler execution.
#[derive(Debug, Clone, Default)]
pub struct TxInfo {
    /// Raw L1 fee in wei, computed from diff_size * l1_fee_rate.
    pub l1_fee: U256,
    /// Estimated diff size in bytes (after compression weighting).
    pub diff_size: u64,
}

/// Chain extension stored in `Context.chain`.
/// Provides the L1 fee rate and collects tx-level L1 fee info.
#[derive(Debug, Clone, Default)]
pub struct CitreaChain {
    /// L1 fee rate (wei per byte of DA data).
    pub l1_fee_rate: u128,
    /// Per-transaction info populated after execution.
    pub tx_info: TxInfo,
}

// ── Context + EVM types ─────────────────────────────────────────────

/// Context type with CitreaChain as the chain extension.
pub type CitreaHandlerContext<DB> =
    Context<BlockEnv, revm::context::TxEnv, CfgEnv<MainnetSpecId>, DB, Journal<DB>, CitreaChain>;

/// Citrea handler EVM wrapping the revm EvmCtx with CitreaChain.
#[allow(missing_debug_implementations)]
pub struct CitreaHandlerEvm<DB: revm::database::Database, I> {
    pub inner: EvmCtx<
        CitreaHandlerContext<DB>,
        I,
        EthInstructions<EthInterpreter, CitreaHandlerContext<DB>>,
        PrecompilesMap,
        EthFrame,
    >,
}

impl<DB: Database, I> CitreaHandlerEvm<DB, I> {
    /// Creates a new [`CitreaHandlerEvm`].
    pub fn new(
        block_env: BlockEnv,
        cfg: CfgEnv<CitreaHardfork>,
        db: DB,
        inspector: I,
        l1_fee_rate: u128,
    ) -> Self {
        let spec = cfg.spec;
        let precompiles = PrecompilesMap::from_static(CitreaPrecompiles::new(spec).precompiles());
        let mainnet_cfg = cfg.with_spec_and_mainnet_gas_params(MainnetSpecId::from(spec));

        Self {
            inner: EvmCtx {
                ctx: Context {
                    block: block_env,
                    cfg: mainnet_cfg,
                    journaled_state: Journal::new(db),
                    tx: Default::default(),
                    chain: CitreaChain {
                        l1_fee_rate,
                        tx_info: TxInfo::default(),
                    },
                    local: Default::default(),
                    error: Ok(()),
                },
                inspector,
                instruction: EthInstructions::new_mainnet_with_spec(SpecId::default()),
                precompiles,
                frame_stack: Default::default(),
            },
        }
    }

    /// Returns a reference to the chain extension.
    pub fn citrea_chain(&self) -> &CitreaChain {
        &self.inner.ctx.chain
    }

    /// Returns a reference to the collected tx info after execution.
    pub fn tx_info(&self) -> &TxInfo {
        &self.inner.ctx.chain.tx_info
    }
}

impl<DB: Database, I> Deref for CitreaHandlerEvm<DB, I> {
    type Target = CitreaHandlerContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner.ctx
    }
}

impl<DB: Database, I> DerefMut for CitreaHandlerEvm<DB, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner.ctx
    }
}

// ── EvmTr implementation ────────────────────────────────────────────

impl<DB, INSP> EvmTr for CitreaHandlerEvm<DB, INSP>
where
    DB: Database,
{
    type Context = CitreaHandlerContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, CitreaHandlerContext<DB>>;
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

// ── InspectorEvmTr implementation ───────────────────────────────────

impl<DB, INSP> InspectorEvmTr for CitreaHandlerEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CitreaHandlerContext<DB>, EthInterpreter>,
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
