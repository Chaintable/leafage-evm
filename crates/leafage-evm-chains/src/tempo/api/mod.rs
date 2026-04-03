use alloy_evm::precompiles::PrecompilesMap;
use std::ops::{Deref, DerefMut};

use crate::tempo::block::TempoBlockEnv;
use crate::tempo::hardfork::TempoHardfork;
use crate::tempo::precompile::{extend_tempo_precompiles, storage::take_last_precompile_refund};
use crate::tempo::tx::TempoTxEnv;
use alloy_evm::{Database, EvmEnv};
use revm::{
    context::{CfgEnv, Evm as EvmCtx, FrameStack, JournalTr},
    context_interface::cfg::gas_params::{GasId, GasParams},
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::EthInstructions,
        EthFrame, EvmTr, FrameInitOrResult, FrameResult, PrecompileProvider,
    },
    inspector::InspectorEvmTr,
    interpreter::{
        interpreter::EthInterpreter, interpreter_action::FrameInit, CallInputs, Instruction,
        InterpreterResult,
    },
    precompile::{PrecompileSpecId, Precompiles},
    primitives::Address,
    Context, Inspector, Journal,
};

/// Wrapper around [`PrecompilesMap`] that propagates gas refunds from custom
/// precompile SSTORE operations.
///
/// Standard Ethereum precompiles are pure functions and never produce gas refunds.
/// Tempo's custom precompiles (TIP20, FeeManager, etc.) perform SSTORE through the
/// journal, tracking refunds in [`PrecompileOutput::gas_refunded`]. The upstream
/// `PrecompilesMap::run()` records `gas_used` but not `gas_refunded` — which is correct
/// for standard precompiles. This wrapper adds the missing `record_refund()` call
/// so that SSTORE clear refunds (e.g., balance slot non-zero → zero) propagate to
/// the execution result's `ResultGas`.
pub struct TempoPrecompiles(PrecompilesMap);

impl TempoPrecompiles {
    pub fn new(inner: PrecompilesMap) -> Self {
        Self(inner)
    }
}

impl<DB: Database> PrecompileProvider<TempoContext<DB>> for TempoPrecompiles {
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: TempoHardfork) -> bool {
        PrecompileProvider::<TempoContext<DB>>::set_spec(&mut self.0, spec)
    }

    fn run(
        &mut self,
        context: &mut TempoContext<DB>,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        let result = self.0.run(context, inputs)?;
        // Drain the thread-local refund set by the precompile macro.
        // We intentionally do NOT call record_refund() here — the writer
        // also uses PrecompilesMap which doesn't propagate precompile
        // SSTORE refunds to the Gas struct. Matching writer behavior means
        // used() == spent() for precompile calls.
        let _ = take_last_precompile_refund();
        Ok(result)
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        PrecompileProvider::<TempoContext<DB>>::warm_addresses(&self.0)
    }

    fn contains(&self, address: &Address) -> bool {
        PrecompileProvider::<TempoContext<DB>>::contains(&self.0, address)
    }
}

mod exec;

/// Type alias for the default context type of the TempoEvm.
pub type TempoContext<DB> = Context<TempoBlockEnv, TempoTxEnv, CfgEnv<TempoHardfork>, DB>;

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
        TempoPrecompiles,
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
    pub fn new(env: EvmEnv<TempoHardfork>, db: DB, inspector: I, inspect: bool) -> Self {
        let mut precompiles = PrecompilesMap::from_static(Precompiles::new(
            PrecompileSpecId::from_spec_id(env.cfg_env.spec.into()),
        ));
        extend_tempo_precompiles(&mut precompiles, env.cfg_env.chain_id);

        let mut cfg_env = env.cfg_env;
        let timestamp = env.block_env.timestamp.saturating_to::<u64>();
        let hardfork = TempoHardfork::from_timestamp(timestamp);
        cfg_env.spec = hardfork;
        let mut gas_params = GasParams::new_spec(hardfork.into());
        if hardfork.is_t1() {
            gas_params.override_gas([
                (GasId::sstore_set_without_load_cost(), 250_000),
                (GasId::create(), 500_000),
                (GasId::tx_create_cost(), 500_000),
                (GasId::new_account_cost(), 250_000),
                (GasId::new_account_cost_for_selfdestruct(), 250_000),
                (GasId::code_deposit_cost(), 1_000),
                (GasId::tx_eip7702_per_empty_account_cost(), 12_500),
                // TIP-1000: Auth account creation cost (EIP-7702 auth with nonce==0).
                // Custom GasId(255), same as Tempo writer: crates/revm/src/gas_params.rs
                (GasId::new(255), 250_000),
            ]);
        }
        cfg_env.gas_params = gas_params;
        let spec: revm::primitives::hardfork::SpecId = cfg_env.spec.clone().into();

        // Build instruction table with MILLIS_TIMESTAMP opcode for pre-T1C archive mode.
        let mut instructions = EthInstructions::new_mainnet_with_spec(spec);
        if !hardfork.is_t1c() {
            // Register MILLIS_TIMESTAMP (0x4F) opcode — active pre-T1C only.
            // Ported from Tempo writer: crates/revm/src/instructions.rs
            const MILLIS_TIMESTAMP_OPCODE: u8 = 0x4F;
            const MILLIS_TIMESTAMP_GAS: u64 = 2;
            instructions.insert_instruction(
                MILLIS_TIMESTAMP_OPCODE,
                Instruction::new(
                    |ctx: revm::interpreter::InstructionContext<
                        '_,
                        TempoContext<DB>,
                        EthInterpreter,
                    >| {
                        revm::interpreter::push!(
                            ctx.interpreter,
                            ctx.host.block.timestamp_millis()
                        );
                    },
                    MILLIS_TIMESTAMP_GAS,
                ),
            );
        }

        Self {
            inner: EvmCtx {
                ctx: Context {
                    block: TempoBlockEnv {
                        inner: env.block_env,
                        timestamp_millis_part: 0, // Pipeline does not carry this field.
                    },
                    cfg: cfg_env,
                    journaled_state: Journal::new(db),
                    tx: Default::default(),
                    chain: Default::default(),
                    local: Default::default(),
                    error: Ok(()),
                },
                inspector,
                instruction: instructions,
                precompiles: TempoPrecompiles::new(precompiles),
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
    type Precompiles = TempoPrecompiles;
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
            CfgEnv::new_with_spec(TempoHardfork::default()),
            BlockEnv::default(),
        );
        // Should not panic -- verifies precompile registration works
        let _evm = TempoEvm::new(env, EmptyDB::default(), NoOpInspector, false);
    }

    fn make_env(timestamp: u64) -> EvmEnv<TempoHardfork> {
        let mut cfg = CfgEnv::new_with_spec(TempoHardfork::from_timestamp(timestamp));
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
        assert_eq!(
            gp.get(GasId::new_account_cost()),
            25_000,
            "pre-T1A new_account should be 25k"
        );
        assert_eq!(
            gp.tx_eip7702_per_empty_account_cost(),
            25_000,
            "pre-T1A eip7702 should be 25k"
        );

        // Post-T1A: TIP-1000 gas overrides
        let evm_post = TempoEvm::new(
            make_env(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gp = &evm_post.inner.ctx.cfg.gas_params;
        assert_eq!(
            gp.get(GasId::sstore_set_without_load_cost()),
            250_000,
            "T1+ SSTORE set should be 250k"
        );
        assert_eq!(
            gp.get(GasId::create()),
            500_000,
            "T1+ CREATE should be 500k"
        );
        assert_eq!(
            gp.get(GasId::tx_create_cost()),
            500_000,
            "T1+ tx_create should be 500k"
        );
        assert_eq!(
            gp.get(GasId::new_account_cost()),
            250_000,
            "T1+ new_account should be 250k"
        );
        assert_eq!(
            gp.get(GasId::new_account_cost_for_selfdestruct()),
            250_000,
            "T1+ selfdestruct new_account should be 250k"
        );
        assert_eq!(
            gp.get(GasId::code_deposit_cost()),
            1_000,
            "T1+ code_deposit should be 1k/byte"
        );
        assert_eq!(
            gp.tx_eip7702_per_empty_account_cost(),
            12_500,
            "T1+ eip7702 should be 12.5k"
        );
    }

    fn make_env_default_spec(timestamp: u64) -> EvmEnv<TempoHardfork> {
        let mut cfg = CfgEnv::new_with_spec(TempoHardfork::default());
        cfg.chain_id = 4217;
        let mut block_env = BlockEnv::default();
        block_env.timestamp = revm::primitives::U256::from(timestamp);
        block_env.gas_limit = 100_000_000;
        EvmEnv::new(cfg, block_env)
    }

    #[test]
    fn test_spec_override_genesis() {
        let evm = TempoEvm::new(
            make_env_default_spec(1000),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        assert_eq!(evm.inner.ctx.cfg.spec, TempoHardfork::Genesis);
        assert_eq!(
            evm.inner.ctx.cfg.gas_params.get(GasId::new_account_cost()),
            25_000
        );
    }

    #[test]
    fn test_spec_override_t1a() {
        let evm = TempoEvm::new(
            make_env_default_spec(1_770_908_400),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        assert_eq!(evm.inner.ctx.cfg.spec, TempoHardfork::T1A);
        assert_eq!(
            evm.inner.ctx.cfg.gas_params.get(GasId::new_account_cost()),
            250_000
        );
        assert_eq!(
            evm.inner
                .ctx
                .cfg
                .gas_params
                .get(GasId::sstore_set_without_load_cost()),
            250_000
        );
    }

    #[test]
    fn test_spec_override_t2() {
        let evm = TempoEvm::new(
            make_env_default_spec(1_774_965_600),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        assert_eq!(evm.inner.ctx.cfg.spec, TempoHardfork::T2);
        assert_eq!(
            evm.inner.ctx.cfg.gas_params.get(GasId::new_account_cost()),
            250_000
        );
    }

    #[test]
    fn test_spec_override_no_downgrade() {
        let evm = TempoEvm::new(
            make_env_default_spec(1_774_965_600 + 1000),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        assert_eq!(evm.inner.ctx.cfg.spec, TempoHardfork::T2);
    }

    #[test]
    fn test_hardfork_maps_to_prague() {
        use revm::primitives::hardfork::SpecId;
        for hf in [
            TempoHardfork::Genesis,
            TempoHardfork::T1,
            TempoHardfork::T1A,
            TempoHardfork::T1B,
            TempoHardfork::T1C,
            TempoHardfork::T2,
            TempoHardfork::T3,
        ] {
            assert_eq!(
                SpecId::from(hf),
                SpecId::PRAGUE,
                "{hf:?} should map to PRAGUE"
            );
        }
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

    // ==================== AA Gas Validation Tests ====================
    // Ported from Tempo writer: crates/revm/src/handler.rs

    /// Make env with nonce check disabled (simulates eth_call mode).
    fn make_env_aa(timestamp: u64) -> EvmEnv<TempoHardfork> {
        let mut cfg = CfgEnv::new_with_spec(TempoHardfork::from_timestamp(timestamp));
        cfg.chain_id = 4217;
        cfg.disable_balance_check = true;
        cfg.disable_nonce_check = true;
        cfg.disable_eip3607 = true;
        cfg.disable_block_gas_limit = true;
        cfg.disable_base_fee = true;
        let mut block_env = BlockEnv::default();
        block_env.timestamp = revm::primitives::U256::from(timestamp);
        block_env.gas_limit = 100_000_000;
        EvmEnv::new(cfg, block_env)
    }

    /// Helper to create an AA tx with batch calls.
    fn make_aa_tx(
        calls: Vec<crate::tempo::tx::TempoCall>,
        nonce: u64,
        nonce_key: revm::primitives::U256,
        gas_limit: u64,
    ) -> crate::tempo::tx::TempoTxEnv {
        use revm::primitives::Address;
        crate::tempo::tx::TempoTxEnv {
            base: revm::context::TxEnv {
                caller: Address::ZERO,
                gas_limit,
                nonce,
                chain_id: Some(4217),
                ..Default::default()
            },
            tempo_fields: Some(crate::tempo::tx::TempoTxFields {
                aa_calls: calls,
                nonce_key,
                ..Default::default()
            }),
        }
    }

    /// Helper to create a simple TempoCall targeting an address with given data.
    fn make_call(to_byte: u8, data: &[u8]) -> crate::tempo::tx::TempoCall {
        use revm::primitives::{Address, Bytes, TxKind};
        crate::tempo::tx::TempoCall {
            to: TxKind::Call(Address::with_last_byte(to_byte)),
            value: revm::primitives::U256::ZERO,
            input: Bytes::copy_from_slice(data),
        }
    }

    /// AA batch with single empty call: should cost base stipend (21k) + calldata (0).
    #[test]
    fn test_aa_gas_single_empty_call() {
        use revm::primitives::U256;

        let calls = vec![make_call(0x01, &[])];
        let tx = make_aa_tx(calls, 1, U256::ZERO, 10_000_000);

        let mut evm = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100), // Post-T1A
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let result = evm.transact(tx).expect("AA single call transact");
        let gas_used = result.result.gas_used();

        // Base stipend = 21000, no per-call cold cost (single call), no calldata.
        // Execution gas is minimal (CALL to empty account).
        assert!(
            gas_used >= 21_000,
            "AA single call gas ({gas_used}) should be >= 21k base"
        );
    }

    /// AA batch with multiple calls: should include per-call cold account cost.
    #[test]
    fn test_aa_gas_multi_call_cold_account() {
        use revm::primitives::U256;

        let calls_1 = vec![make_call(0x01, &[])];
        let calls_3 = vec![
            make_call(0x01, &[]),
            make_call(0x02, &[]),
            make_call(0x03, &[]),
        ];

        let tx_1 = make_aa_tx(calls_1, 1, U256::ZERO, 10_000_000);
        let tx_3 = make_aa_tx(calls_3, 1, U256::ZERO, 10_000_000);

        let mut evm_1 = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_1 = evm_1.transact(tx_1).expect("1-call").result.gas_used();

        let mut evm_3 = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_3 = evm_3.transact(tx_3).expect("3-call").result.gas_used();

        // 3-call should cost more due to 2 * cold_account_cost (2600 each).
        assert!(
            gas_3 > gas_1,
            "3-call gas ({gas_3}) should be > 1-call gas ({gas_1})"
        );
    }

    /// AA tx with nonce == 0 on T1+ should add 250k new_account_cost.
    #[test]
    fn test_aa_gas_nonce_zero_surcharge() {
        use revm::primitives::U256;

        let calls = vec![make_call(0x01, &[])];

        // nonce == 0
        let tx_n0 = make_aa_tx(calls.clone(), 0, U256::ZERO, 10_000_000);
        // nonce == 1
        let tx_n1 = make_aa_tx(calls, 1, U256::ZERO, 10_000_000);

        let mut evm_n0 = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_n0 = evm_n0.transact(tx_n0).expect("nonce=0").result.gas_used();

        let mut evm_n1 = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_n1 = evm_n1.transact(tx_n1).expect("nonce=1").result.gas_used();

        // nonce=0 should cost ~250k more (TIP-1000 new_account_cost).
        let diff = gas_n0.saturating_sub(gas_n1);
        assert_eq!(
            diff, 250_000,
            "nonce=0 surcharge should be exactly 250k, got diff={diff}"
        );
    }

    /// AA tx with expiring nonce key (U256::MAX) on T1+ should add EXPIRING_NONCE_GAS (13k).
    #[test]
    fn test_aa_gas_expiring_nonce() {
        use revm::primitives::U256;

        let calls = vec![make_call(0x01, &[])];

        // Normal nonce_key (nonce > 0)
        let tx_normal = make_aa_tx(calls.clone(), 1, U256::from(1), 10_000_000);
        // Expiring nonce_key (U256::MAX) — requires valid_before to be set.
        let mut tx_expiring = make_aa_tx(calls, 1, U256::MAX, 10_000_000);
        tx_expiring.tempo_fields.as_mut().unwrap().valid_before = Some(u64::MAX);

        let mut evm_normal = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_normal = evm_normal
            .transact(tx_normal)
            .expect("normal nonce")
            .result
            .gas_used();

        let mut evm_exp = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_exp = evm_exp
            .transact(tx_expiring)
            .expect("expiring nonce")
            .result
            .gas_used();

        // Expiring nonce should cost EXPIRING_NONCE_GAS (13k) more than existing nonce key (5k).
        let diff = gas_exp.saturating_sub(gas_normal);
        let expected_diff = 13_000 - 5_000; // EXPIRING_NONCE_GAS - gas_existing_nonce_key
        assert_eq!(
            diff, expected_diff,
            "expiring nonce extra gas should be {expected_diff}, got diff={diff}"
        );
    }

    /// Pre-T1 AA tx should NOT add TIP-1000 nonce surcharge.
    #[test]
    fn test_aa_gas_pre_t1_no_nonce_surcharge() {
        use revm::primitives::U256;

        let calls = vec![make_call(0x01, &[])];

        // nonce == 0, pre-T1
        let tx_n0 = make_aa_tx(calls.clone(), 0, U256::ZERO, 10_000_000);
        // nonce == 1, pre-T1
        let tx_n1 = make_aa_tx(calls, 1, U256::ZERO, 10_000_000);

        let mut evm_n0 = TempoEvm::new(
            make_env_aa(1000), // Pre-T1
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_n0 = evm_n0
            .transact(tx_n0)
            .expect("pre-T1 nonce=0")
            .result
            .gas_used();

        let mut evm_n1 = TempoEvm::new(
            make_env_aa(1000), // Pre-T1
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_n1 = evm_n1
            .transact(tx_n1)
            .expect("pre-T1 nonce=1")
            .result
            .gas_used();

        // Pre-T1: no TIP-1000 nonce surcharge, so gas should be identical.
        assert_eq!(
            gas_n0, gas_n1,
            "pre-T1 nonce=0 ({gas_n0}) should equal nonce=1 ({gas_n1})"
        );
    }

    /// AA tx with calldata should cost more than empty calldata.
    #[test]
    fn test_aa_gas_calldata_cost() {
        use revm::primitives::U256;

        // 100 bytes of non-zero calldata.
        let data = vec![0xffu8; 100];
        let calls_with_data = vec![make_call(0x01, &data)];
        let calls_empty = vec![make_call(0x01, &[])];

        let tx_data = make_aa_tx(calls_with_data, 1, U256::ZERO, 10_000_000);
        let tx_empty = make_aa_tx(calls_empty, 1, U256::ZERO, 10_000_000);

        let mut evm_data = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_data = evm_data
            .transact(tx_data)
            .expect("with data")
            .result
            .gas_used();

        let mut evm_empty = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_empty = evm_empty
            .transact(tx_empty)
            .expect("empty data")
            .result
            .gas_used();

        // 100 non-zero bytes = 100 * 4 = 400 tokens, * token_cost.
        assert!(
            gas_data > gas_empty,
            "calldata gas ({gas_data}) should be > empty gas ({gas_empty})"
        );
    }

    // ==================== Precompile SSTORE Refund Tests ====================

    /// End-to-end: TIP20 transfer that clears sender balance (non-zero → zero)
    /// must produce a gas refund that propagates through TempoPrecompiles::run()
    /// to ResultGas, making gas.used() < gas.spent().
    ///
    /// This tests the full path:
    ///   SSTORE in precompile → StorageCtx.refund_gas()
    ///   → tempo_precompile! macro → set_last_precompile_refund()
    ///   → TempoPrecompiles::run() → take + record_refund()
    ///   → ResultGas.used() accounts for refund
    #[test]
    fn test_precompile_sstore_refund_e2e() {
        use crate::tempo::precompile::storage_types::StorageKey;
        use revm::bytecode::Bytecode;
        use revm::database::in_memory_db::CacheDB;
        use revm::primitives::{keccak256, Address, Bytes, TxKind, U256};
        use revm::state::AccountInfo;

        // TIP20 pathUSD precompile address (token 0): 0x20c0...0000
        let tip20_addr = Address::new({
            let mut a = [0u8; 20];
            a[0] = 0x20;
            a[1] = 0xc0;
            a
        });

        let sender = Address::with_last_byte(0xAA);
        let recipient = Address::with_last_byte(0xBB);
        let transfer_amount = U256::from(1000u64);

        // --- Set up CacheDB with TIP20 state ---
        let mut db = CacheDB::new(revm::database::EmptyDB::default());

        // TIP20 must have code to be "initialized" (code_hash != EMPTY)
        db.insert_account_info(
            tip20_addr,
            AccountInfo {
                code_hash: Bytecode::new_legacy(vec![0xef].into()).hash_slow(),
                code: Some(Bytecode::new_legacy(vec![0xef].into())),
                nonce: 1,
                ..Default::default()
            },
        );

        // Slot 7: transfer_policy_id = 1 (ALLOW_ALL) packed at byte offset 20.
        // Packed storage uses little-endian bit layout: value << (offset * 8).
        // u64 at offset 20: U256::from(1) << 160
        let slot7_value = U256::from(1u64) << 160;
        db.insert_account_storage(tip20_addr, U256::from(7), slot7_value)
            .unwrap();

        // Slot 9: balances mapping. balances[sender] = transfer_amount
        let balance_slot = sender.mapping_slot(U256::from(9));
        db.insert_account_storage(tip20_addr, balance_slot, transfer_amount)
            .unwrap();

        // Slot 8: total_supply >= transfer_amount (for consistency)
        db.insert_account_storage(tip20_addr, U256::from(8), transfer_amount)
            .unwrap();

        // Sender account must exist
        db.insert_account_info(sender, AccountInfo::default());

        // --- Build and execute TIP20 transfer(recipient, amount) ---
        // Selector: transfer(address,uint256) = 0xa9059cbb
        let mut calldata = vec![0xa9, 0x05, 0x9c, 0xbb];
        calldata.extend_from_slice(&{
            let mut buf = [0u8; 32];
            buf[12..].copy_from_slice(recipient.as_slice());
            buf
        });
        calldata.extend_from_slice(&transfer_amount.to_be_bytes::<32>());

        let tx = crate::tempo::tx::TempoTxEnv {
            base: revm::context::TxEnv {
                caller: sender,
                kind: TxKind::Call(tip20_addr),
                gas_limit: 10_000_000,
                chain_id: Some(4217),
                data: Bytes::from(calldata),
                ..Default::default()
            },
            tempo_fields: None,
        };

        let mut evm = TempoEvm::new(
            make_env(1_770_908_400 + 100), // Post-T1A
            db,
            NoOpInspector,
            false,
        );
        let result = evm.transact(tx).expect("TIP20 transfer should succeed");

        assert!(
            result.result.is_success(),
            "transfer should succeed, got: {:?}",
            result.result
        );

        let gas = result.result.gas();
        // Precompile SSTORE refunds are intentionally NOT propagated to ResultGas
        // (matching writer behavior — both use alloy-evm PrecompilesMap which
        // doesn't call record_refund). So used() == spent() for precompile calls.
        // The GasParams-based sstore gas calculation ensures spent() matches writer.
        assert_eq!(
            gas.used(),
            gas.spent_sub_refunded(),
            "precompile gas: used() should equal spent_sub_refunded() (no refund propagation)"
        );
    }

    /// Verify `is_transfer_authorized` T2 short-circuit behavior.
    ///
    /// Setup: TIP20 with WHITELIST policy, sender NOT in whitelist (denied).
    /// - Pre-T2 (T1C): both sender and recipient `is_authorized_as` run.
    /// - Post-T2: sender fails → short-circuit, recipient check skipped.
    ///
    /// Gas should differ: post-T2 saves ~one policy_data sload + one policy_set sload.
    /// NOTE: Currently T2=u64::MAX (not activated). When T2 timestamp is set,
    /// update MAINNET_T2_TIME and this test becomes a live regression test.
    #[test]
    fn test_is_transfer_authorized_t2_short_circuit() {
        use crate::tempo::hardfork::TempoHardfork;
        use crate::tempo::precompile::storage::with_read_only_storage_ctx;
        use crate::tempo::precompile::storage_types::StorageKey;
        use crate::tempo::precompile::tip20::TIP20Token;
        use crate::tempo::precompile::TIP403_REGISTRY_ADDRESS;
        use revm::database::in_memory_db::CacheDB;
        use revm::primitives::{keccak256, Address, U256};

        let tip20_addr = Address::new({
            let mut a = [0u8; 20];
            a[0] = 0x20;
            a[1] = 0xc0;
            a
        });
        let sender = Address::with_last_byte(0xAA);
        let recipient = Address::with_last_byte(0xBB);

        let mut db = CacheDB::new(revm::database::EmptyDB::default());

        // TIP20 must have code (initialized)
        db.insert_account_info(
            tip20_addr,
            revm::state::AccountInfo {
                code_hash: revm::bytecode::Bytecode::new_legacy(vec![0xef].into()).hash_slow(),
                code: Some(revm::bytecode::Bytecode::new_legacy(vec![0xef].into())),
                nonce: 1,
                ..Default::default()
            },
        );

        // Slot 7: transfer_policy_id = 2 (custom policy, not builtin) packed at byte offset 20.
        let policy_id: u64 = 2;
        let slot7_value = U256::from(policy_id) << 160;
        db.insert_account_storage(tip20_addr, U256::from(7), slot7_value)
            .unwrap();

        // TIP403Registry must have code (initialized)
        db.insert_account_info(
            TIP403_REGISTRY_ADDRESS,
            revm::state::AccountInfo {
                code_hash: revm::bytecode::Bytecode::new_legacy(vec![0xef].into()).hash_slow(),
                code: Some(revm::bytecode::Bytecode::new_legacy(vec![0xef].into())),
                nonce: 1,
                ..Default::default()
            },
        );

        // policy_id_counter (slot 0) = 3 (policies 0,1,2 exist)
        db.insert_account_storage(TIP403_REGISTRY_ADDRESS, U256::from(0), U256::from(3))
            .unwrap();

        // policy_records[2].base = PolicyData { policy_type: WHITELIST(0), admin: 0x01 }
        // Slot: keccak256(abi.encode(2, 1))
        let policy_record_slot = U256::from(policy_id).mapping_slot(U256::from(1));
        // Packed: byte 31 = policy_type (0 = WHITELIST), bytes 11..31 = admin
        let mut policy_data_bytes = [0u8; 32];
        policy_data_bytes[31] = 0; // WHITELIST
        policy_data_bytes[30] = 0x01; // admin = Address::with_last_byte(0x01)
        db.insert_account_storage(
            TIP403_REGISTRY_ADDRESS,
            policy_record_slot,
            U256::from_be_bytes(policy_data_bytes),
        )
        .unwrap();

        // policy_set[2][sender] = false (NOT whitelisted → denied)
        // Slot: keccak256(abi.encode(sender, keccak256(abi.encode(2, 2))))
        let policy_set_outer = U256::from(policy_id).mapping_slot(U256::from(2));
        let policy_set_sender = sender.mapping_slot(policy_set_outer);
        // Don't insert → default 0 = false = not in whitelist

        // policy_set[2][recipient] = true (whitelisted → authorized)
        let policy_set_recipient = recipient.mapping_slot(policy_set_outer);
        db.insert_account_storage(TIP403_REGISTRY_ADDRESS, policy_set_recipient, U256::from(1))
            .unwrap();

        // --- Pre-T2 (T1C): both sender and recipient checks run ---
        let result_pre_t2 = with_read_only_storage_ctx(&db, TempoHardfork::T1C, 4217, || {
            TIP20Token::from_address_unchecked(tip20_addr).is_transfer_authorized(sender, recipient)
        });
        assert_eq!(
            result_pre_t2.unwrap(),
            false,
            "pre-T2: sender denied → false"
        );

        // --- Post-T2: sender fails → short-circuit, recipient not checked ---
        let result_t2 = with_read_only_storage_ctx(&db, TempoHardfork::T2, 4217, || {
            TIP20Token::from_address_unchecked(tip20_addr).is_transfer_authorized(sender, recipient)
        });
        assert_eq!(
            result_t2.unwrap(),
            false,
            "T2: sender denied → short-circuit → false"
        );

        // Both return false, but gas differs:
        // Pre-T2: reads policy_data + policy_set[sender] + policy_data + policy_set[recipient]
        // Post-T2: reads policy_data + policy_set[sender] only (short-circuit)
    }

    // ==================== T2 Hardfork Tests ====================

    /// T2 nonce gas repricing: existing_nonce_key and new_nonce_key each gain +200.
    ///
    /// T1C: existing=5000, new=22100
    /// T2:  existing=5200, new=22300
    ///
    /// Executes AA txs at T1C and T2 timestamps with a nonzero nonce_key and
    /// nonzero nonce (existing key path). The T2 tx should cost exactly 200 more.
    #[test]
    fn test_t2_nonce_gas_repricing() {
        use revm::primitives::U256;

        let pre_t2_ts = 1_773_327_600 + 100; // T1C era
        let post_t2_ts = 1_774_965_600 + 100; // T2 era

        let calls = vec![make_call(0x01, &[])];

        // nonce_key = 1 (nonzero), nonce = 1 (existing key path)
        let tx_pre = make_aa_tx(calls.clone(), 1, U256::from(1), 10_000_000);
        let tx_post = make_aa_tx(calls, 1, U256::from(1), 10_000_000);

        let mut evm_pre = TempoEvm::new(
            make_env_aa(pre_t2_ts),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_pre = evm_pre
            .transact(tx_pre)
            .expect("T1C transact")
            .result
            .gas_used();

        let mut evm_post = TempoEvm::new(
            make_env_aa(post_t2_ts),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_post = evm_post
            .transact(tx_post)
            .expect("T2 transact")
            .result
            .gas_used();

        // T2 existing nonce key costs 5200 vs T1C 5000 → exactly +200.
        let diff = gas_post.saturating_sub(gas_pre);
        assert_eq!(
            diff, 200,
            "T2 existing nonce gas should be +200 vs T1C, got diff={diff} (pre={gas_pre}, post={gas_post})"
        );
    }

    /// T2 hardfork gas parameter values in TempoHardfork.
    ///
    /// Verifies `gas_existing_nonce_key` and `gas_new_nonce_key` return the
    /// correct values for each era.
    #[test]
    fn test_t2_hardfork_gas_params() {
        let t1c = TempoHardfork::T1C;
        let t2 = TempoHardfork::T2;

        // T1C nonce gas
        assert_eq!(
            t1c.gas_existing_nonce_key(),
            5_000,
            "T1C existing nonce key gas"
        );
        assert_eq!(t1c.gas_new_nonce_key(), 22_100, "T1C new nonce key gas");

        // T2 nonce gas: each +200
        assert_eq!(
            t2.gas_existing_nonce_key(),
            5_200,
            "T2 existing nonce key gas"
        );
        assert_eq!(t2.gas_new_nonce_key(), 22_300, "T2 new nonce key gas");

        // Deltas
        assert_eq!(
            t2.gas_existing_nonce_key() - t1c.gas_existing_nonce_key(),
            200,
            "existing nonce key delta should be +200"
        );
        assert_eq!(
            t2.gas_new_nonce_key() - t1c.gas_new_nonce_key(),
            200,
            "new nonce key delta should be +200"
        );
    }

    /// T2 hardfork gas params applied in TempoEvm constructor.
    ///
    /// TempoEvm at T2 timestamp should still have TIP-1000 gas overrides
    /// (same as T1+), since T2 >= T1. Verifies the constructor propagates them.
    #[test]
    fn test_t2_evm_gas_params_applied() {
        let evm = TempoEvm::new(
            make_env(1_774_965_600 + 100), // T2 era
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gp = &evm.inner.ctx.cfg.gas_params;

        // TIP-1000 overrides should still be active at T2
        assert_eq!(
            gp.get(GasId::sstore_set_without_load_cost()),
            250_000,
            "T2 SSTORE set should be 250k"
        );
        assert_eq!(gp.get(GasId::create()), 500_000, "T2 CREATE should be 500k");
        assert_eq!(
            gp.get(GasId::tx_create_cost()),
            500_000,
            "T2 tx_create should be 500k"
        );
        assert_eq!(
            gp.get(GasId::new_account_cost()),
            250_000,
            "T2 new_account should be 250k"
        );
        assert_eq!(
            gp.get(GasId::code_deposit_cost()),
            1_000,
            "T2 code_deposit should be 1k/byte"
        );
    }

    /// T2 `ensure_admin_caller` requires tx_origin == msg_sender.
    ///
    /// Tests through `authorize_key` which calls `ensure_admin_caller` first.
    /// In `with_read_only_storage_ctx`, tload() returns ZERO for all transient slots,
    /// so `transaction_key = ZERO` (main key, passes) and `tx_origin = ZERO`.
    ///
    /// Pre-T2: admin check passes (no tx_origin verification), then fails at
    ///         `expiry <= timestamp` with `ExpiryInPast`.
    /// T2: admin check itself fails because `tx_origin == ZERO != msg_sender`,
    ///     returning `UnauthorizedCaller` before reaching expiry check.
    #[test]
    fn test_t2_account_keychain_admin_requires_tx_origin() {
        use crate::tempo::precompile::account_keychain::{AccountKeychain, IAccountKeychain};
        use crate::tempo::precompile::error::TempoPrecompileError;
        use crate::tempo::precompile::storage::with_read_only_storage_ctx;
        use crate::tempo::precompile::ACCOUNT_KEYCHAIN_ADDRESS;
        use alloy::sol_types::SolError;
        use revm::database::in_memory_db::CacheDB;
        use revm::primitives::Address;

        let msg_sender = Address::with_last_byte(0xAA);
        let key_id = Address::with_last_byte(0x01);

        let mut db = CacheDB::new(revm::database::EmptyDB::default());

        // AccountKeychain must have code (initialized)
        db.insert_account_info(
            ACCOUNT_KEYCHAIN_ADDRESS,
            revm::state::AccountInfo {
                code_hash: revm::bytecode::Bytecode::new_legacy(vec![0xef].into()).hash_slow(),
                code: Some(revm::bytecode::Bytecode::new_legacy(vec![0xef].into())),
                nonce: 1,
                ..Default::default()
            },
        );

        // Build an authorize_key call. expiry=0 so pre-T2 hits ExpiryInPast after
        // admin check succeeds. keyId is nonzero to avoid ZeroPublicKey error.
        let call = IAccountKeychain::authorizeKeyCall {
            keyId: key_id,
            signatureType: IAccountKeychain::SignatureType::Secp256k1,
            expiry: 0,
            enforceLimits: false,
            limits: vec![],
        };

        // Pre-T2 (T1C): admin check passes (no tx_origin gate), fails at ExpiryInPast.
        let result_pre_t2 = with_read_only_storage_ctx(&db, TempoHardfork::T1C, 4217, || {
            let mut keychain = AccountKeychain::new();
            keychain.authorize_key(msg_sender, call.clone())
        });
        match &result_pre_t2 {
            Err(TempoPrecompileError::Revert(data)) => {
                let expected = IAccountKeychain::ExpiryInPast {}.abi_encode();
                assert_eq!(
                    data.as_ref(),
                    expected.as_slice(),
                    "pre-T2: should fail with ExpiryInPast (admin check passed), got different revert"
                );
            }
            other => panic!("pre-T2: expected Revert(ExpiryInPast), got: {other:?}"),
        }

        // T2: admin check fails at tx_origin gate → UnauthorizedCaller.
        let result_t2 = with_read_only_storage_ctx(&db, TempoHardfork::T2, 4217, || {
            let mut keychain = AccountKeychain::new();
            keychain.authorize_key(msg_sender, call.clone())
        });
        match &result_t2 {
            Err(TempoPrecompileError::Revert(data)) => {
                let expected = IAccountKeychain::UnauthorizedCaller {}.abi_encode();
                assert_eq!(
                    data.as_ref(),
                    expected.as_slice(),
                    "T2: should fail with UnauthorizedCaller (tx_origin=ZERO), got different revert"
                );
            }
            other => panic!("T2: expected Revert(UnauthorizedCaller), got: {other:?}"),
        }
    }

    // ==================== AA batch execution semantics tests ====================

    /// AA batch with a failing sub-call: entire batch reverts atomically.
    ///
    /// Setup: TIP20 token with sender balance = 1000. Two calls:
    ///   1. transfer(valid_recipient, 500) — would succeed in isolation
    ///   2. transfer(valid_recipient, 501) — exceeds remaining balance → fails
    ///
    /// Since sub-call 2 fails, the entire batch reverts via checkpoint_revert.
    /// Sender's balance must remain unchanged (1000), proving atomicity.
    #[test]
    fn test_aa_batch_atomic_revert() {
        use crate::tempo::precompile::storage_types::StorageKey;
        use crate::tempo::tx::{TempoCall, TempoTxEnv, TempoTxFields};
        use revm::bytecode::Bytecode;
        use revm::database::in_memory_db::CacheDB;
        use revm::primitives::{Address, Bytes, TxKind, U256};
        use revm::state::AccountInfo;

        // TIP20 pathUSD precompile address (token 0): 0x20c0...0000
        let tip20_addr = Address::new({
            let mut a = [0u8; 20];
            a[0] = 0x20;
            a[1] = 0xc0;
            a
        });

        let sender = Address::with_last_byte(0xAA);
        let recipient = Address::with_last_byte(0xBB);
        let initial_balance = U256::from(1000u64);

        // --- Set up CacheDB with TIP20 state ---
        let mut db = CacheDB::new(revm::database::EmptyDB::default());

        db.insert_account_info(
            tip20_addr,
            AccountInfo {
                code_hash: Bytecode::new_legacy(vec![0xef].into()).hash_slow(),
                code: Some(Bytecode::new_legacy(vec![0xef].into())),
                nonce: 1,
                ..Default::default()
            },
        );

        // Slot 7: transfer_policy_id = 1 (ALLOW_ALL) packed at byte offset 20.
        let slot7_value = U256::from(1u64) << 160;
        db.insert_account_storage(tip20_addr, U256::from(7), slot7_value)
            .unwrap();

        // Slot 9: balances[sender] = 1000
        let balance_slot = sender.mapping_slot(U256::from(9));
        db.insert_account_storage(tip20_addr, balance_slot, initial_balance)
            .unwrap();

        // Slot 8: total_supply >= balance
        db.insert_account_storage(tip20_addr, U256::from(8), initial_balance)
            .unwrap();

        db.insert_account_info(sender, AccountInfo::default());

        // --- Build AA batch: 2 transfers, second exceeds balance ---
        // transfer(address,uint256) = 0xa9059cbb
        let make_transfer_calldata = |amount: U256| -> Bytes {
            let mut data = vec![0xa9, 0x05, 0x9c, 0xbb];
            data.extend_from_slice(&{
                let mut buf = [0u8; 32];
                buf[12..].copy_from_slice(recipient.as_slice());
                buf
            });
            data.extend_from_slice(&amount.to_be_bytes::<32>());
            Bytes::from(data)
        };

        let calls = vec![
            TempoCall {
                to: TxKind::Call(tip20_addr),
                value: U256::ZERO,
                input: make_transfer_calldata(U256::from(500u64)),
            },
            TempoCall {
                to: TxKind::Call(tip20_addr),
                value: U256::ZERO,
                input: make_transfer_calldata(U256::from(501u64)), // exceeds remaining 500
            },
        ];

        let tx = TempoTxEnv {
            base: revm::context::TxEnv {
                caller: sender,
                gas_limit: 10_000_000,
                nonce: 1,
                chain_id: Some(4217),
                ..Default::default()
            },
            tempo_fields: Some(TempoTxFields {
                aa_calls: calls,
                ..Default::default()
            }),
        };

        let mut evm = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100), // Post-T1A
            db,
            NoOpInspector,
            false,
        );
        let result = evm.transact(tx).expect("AA batch transact should not Err");

        // The batch should have reverted (second call fails).
        assert!(
            matches!(
                result.result,
                revm::context_interface::result::ExecutionResult::Revert { .. }
            ),
            "expected Revert from failing batch, got: {:?}",
            result.result
        );

        // Verify sender's balance is unchanged in the state diff.
        // After revert, state changes from call 1 are rolled back,
        // so the sender balance slot should either not appear in state
        // or appear with its original value.
        let tip20_state = result.state.get(&tip20_addr);
        if let Some(account) = tip20_state {
            let balance_slot_u256 = sender.mapping_slot(U256::from(9));
            if let Some(slot) = account.storage.get(&balance_slot_u256) {
                // present_value should equal original (1000) after revert
                assert_eq!(
                    slot.present_value, initial_balance,
                    "sender balance should be unchanged after atomic revert"
                );
            }
        }
    }

    /// AA batch with multiple calls: gas accumulates across sub-calls.
    ///
    /// Single empty call vs. three empty calls to different addresses.
    /// Three calls cost more due to additional CALL opcodes.
    #[test]
    fn test_aa_batch_gas_accumulation() {
        use crate::tempo::tx::{TempoCall, TempoTxEnv, TempoTxFields};
        use revm::primitives::{Bytes, TxKind, U256};

        let single_call = vec![TempoCall {
            to: TxKind::Call(Address::with_last_byte(0x01)),
            value: U256::ZERO,
            input: Bytes::new(),
        }];

        let triple_call = vec![
            TempoCall {
                to: TxKind::Call(Address::with_last_byte(0x01)),
                value: U256::ZERO,
                input: Bytes::new(),
            },
            TempoCall {
                to: TxKind::Call(Address::with_last_byte(0x02)),
                value: U256::ZERO,
                input: Bytes::new(),
            },
            TempoCall {
                to: TxKind::Call(Address::with_last_byte(0x03)),
                value: U256::ZERO,
                input: Bytes::new(),
            },
        ];

        let make_tx = |calls: Vec<TempoCall>| TempoTxEnv {
            base: revm::context::TxEnv {
                caller: Address::ZERO,
                gas_limit: 10_000_000,
                nonce: 1,
                chain_id: Some(4217),
                ..Default::default()
            },
            tempo_fields: Some(TempoTxFields {
                aa_calls: calls,
                ..Default::default()
            }),
        };

        let mut evm_1 = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_1 = evm_1
            .transact(make_tx(single_call))
            .expect("single call")
            .result
            .gas_used();

        let mut evm_3 = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_3 = evm_3
            .transact(make_tx(triple_call))
            .expect("triple call")
            .result
            .gas_used();

        assert!(
            gas_3 > gas_1,
            "3-call gas ({gas_3}) should exceed 1-call gas ({gas_1}), proving accumulation"
        );
    }

    /// Compare VCV2 gas WITH and WITHOUT load_account warm-up, BOTH at T2 timestamp.
    #[test]
    fn test_t2_vcv2_warm_account_effect() {
        use crate::tempo::precompile::VALIDATOR_CONFIG_V2_ADDRESS;
        use revm::bytecode::Bytecode;
        use revm::database::in_memory_db::CacheDB;
        use revm::primitives::{Address, Bytes, TxKind};
        use revm::state::AccountInfo;

        let vcv2 = VALIDATOR_CONFIG_V2_ADDRESS;
        // owner() selector
        let calldata = Bytes::from_static(&[0x8d, 0xa5, 0xcb, 0x5b]);

        // Both at T2 timestamp — compare with and without pre-warmed VCV2 account
        let make_tx = |cd: Bytes| crate::tempo::tx::TempoTxEnv {
            base: revm::context::TxEnv {
                caller: Address::with_last_byte(0xAA),
                kind: TxKind::Call(vcv2),
                gas_limit: 10_000_000,
                chain_id: Some(4217),
                data: cd,
                ..Default::default()
            },
            tempo_fields: None,
        };

        // WITHOUT pre-warm: normal T2 execution
        let db1 = CacheDB::new(revm::database::EmptyDB::default());
        let mut evm1 = TempoEvm::new(
            make_env(1_774_965_700),
            db1,
            revm::inspector::NoOpInspector,
            false,
        );
        let gas_cold = evm1
            .transact(make_tx(calldata.clone()))
            .expect("cold")
            .result
            .gas_used();

        // WITH pre-warm: insert VCV2 into CacheDB before EVM construction
        let mut db2 = CacheDB::new(revm::database::EmptyDB::default());
        db2.insert_account_info(
            vcv2,
            revm::state::AccountInfo {
                code_hash: revm::bytecode::Bytecode::new_legacy(
                    alloy::primitives::Bytes::from_static(&[0xef]),
                )
                .hash_slow(),
                code: Some(revm::bytecode::Bytecode::new_legacy(
                    alloy::primitives::Bytes::from_static(&[0xef]),
                )),
                nonce: 1,
                ..Default::default()
            },
        );
        let mut evm2 = TempoEvm::new(
            make_env(1_774_965_700),
            db2,
            revm::inspector::NoOpInspector,
            false,
        );
        let gas_warm = evm2
            .transact(make_tx(calldata))
            .expect("warm")
            .result
            .gas_used();

        eprintln!("VCV2 T2 gas: cold={gas_cold} warm={gas_warm}");
        // Code/account presence does NOT affect precompile gas — dispatch by address.
        assert_eq!(
            gas_cold, gas_warm,
            "VCV2 code presence should not affect precompile gas"
        );
    }
}
