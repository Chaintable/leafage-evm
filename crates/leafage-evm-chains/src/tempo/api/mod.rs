use alloy_evm::precompiles::PrecompilesMap;
use std::ops::{Deref, DerefMut};

use crate::tempo::block::TempoBlockEnv;
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
    interpreter::{interpreter::EthInterpreter, Instruction, interpreter_action::FrameInit},
    precompile::{PrecompileSpecId, Precompiles},
    Context, Inspector, Journal,
};

mod exec;

/// Type alias for the default context type of the TempoEvm.
pub type TempoContext<DB> = Context<TempoBlockEnv, TempoTxEnv, CfgEnv<MainnetSpecId>, DB>;

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

        // DEBUG: log to verify this path is hit and timestamp is correct
        eprintln!("[TEMPO DEBUG] TempoEvm::new ts={} hf={:?} is_t1={}", timestamp, hardfork, hardfork.is_t1());

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
                // TIP-1000: Auth account creation cost (EIP-7702 auth with nonce==0).
                // Custom GasId(255), same as Tempo writer: crates/revm/src/gas_params.rs
                (GasId::new(255), 250_000),
            ]);
        }
        cfg_env.gas_params = gas_params;
        eprintln!("[TEMPO DEBUG] gas_params sstore={} new_account={} create={}",
            cfg_env.gas_params.get(GasId::sstore_set_without_load_cost()),
            cfg_env.gas_params.get(GasId::new_account_cost()),
            cfg_env.gas_params.get(GasId::create()));
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

    // ==================== AA Gas Validation Tests ====================
    // Ported from Tempo writer: crates/revm/src/handler.rs

    /// Make env with nonce check disabled (simulates eth_call mode).
    fn make_env_aa(timestamp: u64) -> EvmEnv<MainnetSpecId> {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
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
        // Expiring nonce_key (U256::MAX)
        let tx_expiring = make_aa_tx(calls, 1, U256::MAX, 10_000_000);

        let mut evm_normal = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_normal = evm_normal.transact(tx_normal).expect("normal nonce").result.gas_used();

        let mut evm_exp = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_exp = evm_exp.transact(tx_expiring).expect("expiring nonce").result.gas_used();

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
        let gas_n0 = evm_n0.transact(tx_n0).expect("pre-T1 nonce=0").result.gas_used();

        let mut evm_n1 = TempoEvm::new(
            make_env_aa(1000), // Pre-T1
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_n1 = evm_n1.transact(tx_n1).expect("pre-T1 nonce=1").result.gas_used();

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
        let gas_data = evm_data.transact(tx_data).expect("with data").result.gas_used();

        let mut evm_empty = TempoEvm::new(
            make_env_aa(1_770_908_400 + 100),
            EmptyDB::default(),
            NoOpInspector,
            false,
        );
        let gas_empty = evm_empty.transact(tx_empty).expect("empty data").result.gas_used();

        // 100 non-zero bytes = 100 * 4 = 400 tokens, * token_cost.
        assert!(
            gas_data > gas_empty,
            "calldata gas ({gas_data}) should be > empty gas ({gas_empty})"
        );
    }
}
