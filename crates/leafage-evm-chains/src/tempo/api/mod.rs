use alloy_evm::precompiles::PrecompilesMap;
use std::ops::{Deref, DerefMut};

use crate::tempo::hardfork::TempoHardfork;
use crate::tempo::precompile::extend_tempo_precompiles;
use crate::tempo::tx::TempoTxEnv;
use alloy_evm::{Database, EvmEnv};
use leafage_evm_types::MainnetSpecId;
use revm::{
    context::{BlockEnv, CfgEnv, Evm as EvmCtx, FrameStack, JournalTr},
    context_interface::cfg::gas_params::{GasId, GasParams},
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
pub type TempoContext<DB> = Context<BlockEnv, TempoTxEnv, CfgEnv<MainnetSpecId>, DB>;

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

        // Determine active hardfork from block timestamp for archive mode support.
        let timestamp = env.block_env.timestamp.saturating_to::<u64>();
        let hardfork = TempoHardfork::from_timestamp(timestamp);

        // Apply Tempo TIP-1000 gas parameter overrides via revm 36 GasParams API.
        // TIP-1000 was introduced in T1, so only apply overrides for T1+ blocks.
        let mut cfg_env = env.cfg_env;
        let mut gas_params = GasParams::new_spec(cfg_env.spec.into());
        if hardfork.is_t1() {
            gas_params.override_gas([
                (GasId::sstore_set_without_load_cost(), 250_000),
                (GasId::create(), 500_000),
                (GasId::tx_create_cost(), 500_000),
                (GasId::new_account_cost(), 250_000),
                (GasId::new_account_cost_for_selfdestruct(), 250_000),
                (GasId::code_deposit_cost(), 1_000),
                (GasId::tx_eip7702_per_empty_account_cost(), 12_500),
            ]);
        }
        cfg_env.gas_params = gas_params;
        let spec: revm::primitives::hardfork::SpecId = cfg_env.spec.clone().into();

        Self {
            inner: EvmCtx {
                ctx: Context {
                    block: env.block_env,
                    cfg: cfg_env,
                    journaled_state: Journal::new(db),
                    tx: Default::default(),
                    chain: Default::default(),
                    local: Default::default(),
                    error: Ok(()),
                },
                inspector,
                instruction: EthInstructions::new_mainnet_with_spec(spec),
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
    use revm::context_interface::cfg::gas_params::GasId;
    use revm::database::EmptyDB;
    use revm::inspector::NoOpInspector;
    use revm::ExecuteEvm;

    #[test]
    fn test_tempo_evm_constructs() {
        let env = EvmEnv::new(
            CfgEnv::new_with_spec(MainnetSpecId::PRAGUE),
            BlockEnv::default(),
        );
        // Should not panic -- verifies precompile registration works
        let _evm = TempoEvm::new(env, EmptyDB::default(), NoOpInspector, false);
    }

    fn make_env(timestamp: u64) -> EvmEnv<MainnetSpecId> {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
        cfg.chain_id = 4217;
        cfg.disable_balance_check = true;
        cfg.disable_eip3607 = true;
        cfg.disable_block_gas_limit = true;
        cfg.disable_base_fee = true;
        let mut block_env = BlockEnv::default();
        block_env.timestamp = revm::primitives::U256::from(timestamp);
        block_env.gas_limit = 100_000_000;
        EvmEnv::new(cfg, block_env)
    }

    // ==================== TIP-1000 Gas Parameter Tests ====================
    // Ported from Tempo writer: crates/evm/src/evm.rs

    /// Verify GasParams are correctly set based on hardfork timestamp.
    /// Ported from: test_tempo_evm_applies_gas_params + test_tempo_evm_gas_params_differ_t0_vs_t1
    #[test]
    fn test_gas_params_pre_vs_post_t1a() {
        // Pre-T1A: standard gas (T0/Genesis defaults)
        let evm_pre = TempoEvm::new(make_env(1000), EmptyDB::default(), NoOpInspector, false);
        let gp = &evm_pre.inner.ctx.cfg.gas_params;
        assert_eq!(gp.get(GasId::new_account_cost()), 25_000, "pre-T1A new_account should be 25k");
        assert_eq!(gp.tx_eip7702_per_empty_account_cost(), 25_000, "pre-T1A eip7702 should be 25k");

        // Post-T1A: TIP-1000 gas overrides
        let evm_post = TempoEvm::new(
            make_env(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gp = &evm_post.inner.ctx.cfg.gas_params;
        assert_eq!(gp.get(GasId::sstore_set_without_load_cost()), 250_000, "T1+ SSTORE set should be 250k");
        assert_eq!(gp.get(GasId::create()), 500_000, "T1+ CREATE should be 500k");
        assert_eq!(gp.get(GasId::tx_create_cost()), 500_000, "T1+ tx_create should be 500k");
        assert_eq!(gp.get(GasId::new_account_cost()), 250_000, "T1+ new_account should be 250k");
        assert_eq!(gp.get(GasId::new_account_cost_for_selfdestruct()), 250_000, "T1+ selfdestruct new_account should be 250k");
        assert_eq!(gp.get(GasId::code_deposit_cost()), 1_000, "T1+ code_deposit should be 1k/byte");
        assert_eq!(gp.tx_eip7702_per_empty_account_cost(), 12_500, "T1+ eip7702 should be 12.5k");
    }

    /// Verify actual EVM execution gas differs between pre-T1A and post-T1A.
    /// Deploys a contract that does SSTORE, calls it, checks gas difference.
    /// Ported from Tempo writer pattern; regression test for the instruction table fix
    /// (EthInstructions::new_mainnet_with_spec vs new_mainnet).
    #[test]
    fn test_evm_execution_gas_changes_with_hardfork() {
        use crate::tempo::tx::TempoTxEnv;
        use revm::bytecode::Bytecode;
        use revm::database::in_memory_db::CacheDB;
        use revm::primitives::{Address, TxKind};

        // Bytecode: PUSH1 0x01, PUSH1 0x00, SSTORE, STOP
        let bytecode = Bytecode::new_legacy(vec![0x60, 0x01, 0x60, 0x00, 0x55, 0x00].into());
        let contract_addr = Address::with_last_byte(0xcc);

        let make_db = || {
            let mut db = CacheDB::new(EmptyDB::default());
            db.insert_account_info(
                contract_addr,
                revm::state::AccountInfo {
                    code_hash: bytecode.hash_slow(),
                    code: Some(bytecode.clone()),
                    ..Default::default()
                },
            );
            db
        };

        let make_tx = || TempoTxEnv {
            base: revm::context::TxEnv {
                caller: Address::ZERO,
                kind: TxKind::Call(contract_addr),
                gas_limit: 10_000_000,
                chain_id: Some(4217),
                ..Default::default()
            },
            tempo_fields: None,
        };

        // Pre-T1A
        let mut evm_pre = TempoEvm::new(make_env(1000), make_db(), NoOpInspector, false);
        let result_pre = evm_pre.transact(make_tx()).expect("pre-T1A transact");
        let gas_pre = result_pre.result.gas_used();

        // Post-T1A
        let mut evm_post = TempoEvm::new(
            make_env(1_770_908_400 + 100),
            make_db(),
            NoOpInspector,
            false,
        );
        let result_post = evm_post.transact(make_tx()).expect("post-T1A transact");
        let gas_post = result_post.result.gas_used();

        // Post-T1A SSTORE set should cost ~250k instead of ~20k
        // Diff should be ~230k (250000 - 19900)
        assert!(
            gas_post > gas_pre + 200_000,
            "post-T1A gas ({gas_post}) should be >200k more than pre-T1A ({gas_pre}), proving TIP-1000 is active in execution"
        );
    }
}
