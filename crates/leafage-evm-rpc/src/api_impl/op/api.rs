use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::core::{
    ApiCore, CallExecutor, EvmExecutor, GasFeeHandler, GetHaltReason, GetTransactionError,
    ToJsonRpcError, TxSetter,
};
use crate::api_impl::op::evm::{create_op_evm_from_state, create_op_txn_env};
use crate::api_impl::op::legacy;
use crate::api_impl::op::mantle;
use crate::api_impl::ApiImpl;
use crate::error::rpc_error_with_code;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::op::mantle::MANTLE_CHAIN_ID;
use leafage_evm_chains::op::schedule::op_stack_spec_at;
use leafage_evm_types::{BlockInfo, CallRequest, CfgEnv, DebankErrorCode, OpSpecId};
use op_revm::{OpHaltReason, OpTransaction, OpTransactionError};
use revm::context::result::{EVMError, HaltReason, InvalidTransaction};
use revm::context::{result::ExecutionResult, BlockEnv, TxEnv};
use revm::inspector::NoOpInspector;
use revm::primitives::hardfork::SpecId;
use revm::InspectCommitEvm;
use revm::{DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type OpApiImpl<DB> = ApiImpl<DB, OpSpecId, NoneEvmCustomConfig>;

/// The configured cfg with the fork of the executing block. On a chain with a built-in
/// fork schedule (`op::schedule`) the spec and its gas params follow that schedule, so a
/// historical height runs the rules it was produced under; the other OP-stack chains
/// served by this evm-type keep the configured spec.
fn block_cfg(cfg: &CfgEnv<OpSpecId>, block_env: &BlockEnv) -> CfgEnv<OpSpecId> {
    let mut cfg = cfg.clone();
    if let Some(spec) = op_stack_spec_at(
        cfg.chain_id,
        block_env.number.saturating_to(),
        block_env.timestamp.saturating_to(),
    ) {
        cfg.set_spec_and_mainnet_gas_params(spec);
    }
    cfg
}

impl<DB> GasFeeHandler for OpApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = OpTransaction<TxEnv>;

    /// EIP-7825 (Karst) follows the executing block's fork, not the configured spec.
    /// Mantle never has it: its op-geth skips the cap on OP Stack chains.
    fn consensus_tx_gas_limit_cap(&self, _spec: SpecId, block_env: &BlockEnv) -> u64 {
        if self.evm_cfg.cfg.chain_id == MANTLE_CHAIN_ID {
            return u64::MAX;
        }
        let spec: SpecId = block_cfg(&self.evm_cfg.cfg, block_env).spec.into();
        if spec.is_enabled_in(SpecId::OSAKA) {
            revm::primitives::eip7825::TX_GAS_LIMIT_CAP
        } else {
            u64::MAX
        }
    }

    /// A Mantle gas limit before Arsia is in token-ratio units, and so is the cap.
    fn rpc_tx_gas_limit_cap<StateDB: DatabaseRef>(
        &self,
        configured: Option<u64>,
        block_env: &BlockEnv,
        db: StateDB,
    ) -> Option<u64> {
        mantle::rpc_gas_cap(&self.evm_cfg.cfg, block_env, &db, configured)
    }
}

impl<DB> EvmExecutor for OpApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = OpTransaction<TxEnv>;
    type TransactionError = OpTransactionError;
    type EvmHaltReason = OpHaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        let mut cfg = block_cfg(&self.evm_cfg.cfg, block_env);
        cfg.tx_gas_limit_cap =
            mantle::rpc_gas_cap(&self.evm_cfg.cfg, block_env, &db, cfg.tx_gas_limit_cap);
        create_op_txn_env(block_env, cfg, request, db, chain_id)
    }

    fn transact<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        if legacy::is_legacy(&self.evm_cfg.cfg, block_env) {
            return legacy::transact(&self.evm_cfg.cfg, block_env, state, tx);
        }
        match mantle::fork_at(&self.evm_cfg.cfg, block_env) {
            Some(fork) if fork.scales_gas() => {
                return mantle::transact(
                    &self.evm_cfg.cfg,
                    fork,
                    block_env,
                    state,
                    tx,
                    mantle::L1Fee::Skip,
                );
            }
            Some(_) => {
                let mut evm = create_op_evm_from_state(
                    block_env.clone(),
                    mantle::arsia_cfg(&self.evm_cfg.cfg),
                    state,
                    NoOpInspector {},
                )
                .with_precompiles(mantle::arsia_precompiles());
                return evm.transact(tx).map(|res| res.result.into());
            }
            None => {}
        }
        let mut evm = create_op_evm_from_state(
            block_env.clone(),
            block_cfg(&self.evm_cfg.cfg, block_env),
            state,
            NoOpInspector {},
        );

        evm.transact_read_only(tx).map(Into::into)
    }

    /// Keeps one EVM for the whole run (see the Base executor): the L1 block
    /// info is fetched once per run instead of per call. Pre-Bedrock blocks
    /// keep the per-call legacy path.
    fn with_call_executor<StateDB, R>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        f: impl FnOnce(&mut CallExecutor<'_, Self, StateDB>) -> R,
    ) -> R
    where
        StateDB: DatabaseRef + std::fmt::Debug + Clone,
        StateDB::Error: Sync + Send + 'static,
    {
        if legacy::is_legacy(&self.evm_cfg.cfg, block_env) {
            return f(&mut |tx| legacy::transact(&self.evm_cfg.cfg, block_env, state.clone(), tx));
        }
        let cfg = block_cfg(&self.evm_cfg.cfg, block_env);
        let mut evm = create_op_evm_from_state(block_env.clone(), cfg, state, NoOpInspector {});
        f(&mut |tx| evm.transact_read_only(tx))
    }

    /// A Mantle estimate before Arsia also has to cover the L1 data fee, which
    /// comes out of the gas limit.
    fn transact_for_estimation<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        if let Some(fork) = mantle::scaled_fork_at(&self.evm_cfg.cfg, block_env) {
            return mantle::transact(
                &self.evm_cfg.cfg,
                fork,
                block_env,
                state,
                tx,
                mantle::L1Fee::Estimate,
            );
        }
        self.transact(block_env, state, tx)
    }

    fn inspect_tx_commit<
        StateDB: DatabaseRef + DatabaseCommit,
        R,
        F: FnOnce(TracingInspector) -> R,
    >(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let mut inspector = TracingInspector::new(inspector_cfg);
        if legacy::is_legacy(&self.evm_cfg.cfg, block_env) {
            let res =
                legacy::inspect_tx_commit(&self.evm_cfg.cfg, block_env, state, &mut inspector, tx);
            return res.map(|res| (res, inspector_collect(inspector)));
        }
        match mantle::fork_at(&self.evm_cfg.cfg, block_env) {
            Some(fork) if fork.scales_gas() => {
                let res = mantle::inspect_tx_commit(
                    &self.evm_cfg.cfg,
                    fork,
                    block_env,
                    state,
                    &mut inspector,
                    tx,
                );
                return res.map(|res| (res, inspector_collect(inspector)));
            }
            Some(_) => {
                let mut evm = create_op_evm_from_state(
                    block_env.clone(),
                    mantle::arsia_cfg(&self.evm_cfg.cfg),
                    state,
                    &mut inspector,
                )
                .with_precompiles(mantle::arsia_precompiles());
                let res = evm.inspect_tx_commit(tx);
                return res.map(|res| (res.into(), inspector_collect(inspector)));
            }
            None => {}
        }
        let mut evm = create_op_evm_from_state(
            block_env.clone(),
            block_cfg(&self.evm_cfg.cfg, block_env),
            state,
            &mut inspector,
        );

        let res = evm
            .inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)));

        res
    }
}

impl ToJsonRpcError for OpTransactionError {
    fn to_rpc_error(&self) -> jsonrpsee::types::ErrorObjectOwned {
        match self {
            OpTransactionError::Base(invalid_tx) => invalid_tx.to_rpc_error(),
            e => rpc_error_with_code(DebankErrorCode::EvmFailed as i32, e.to_string()),
        }
    }
}

impl GetTransactionError for OpTransactionError {
    fn get_transaction_error(&self) -> Option<InvalidTransaction> {
        match self {
            OpTransactionError::Base(invalid_tx) => Some(invalid_tx.clone()),
            _ => None,
        }
    }
}

impl TxSetter for OpTransaction<TxEnv> {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.gas_limit = gas_limit;
    }
}

impl GetHaltReason for OpHaltReason {
    fn get_halt_reason(&self) -> Option<HaltReason> {
        match self {
            OpHaltReason::Base(halt) => Some(halt.clone()),
            _ => None,
        }
    }
}

impl<DB> ApiCore for OpApiImpl<DB> where DB: Sync + Send + 'static {}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_chains::op::mainnet::OP_MAINNET_CHAIN_ID;
    use leafage_evm_chains::op::schedule::{
        BOB_CHAIN_ID, CYBER_CHAIN_ID, FRAXTAL_CHAIN_ID, KATANA_CHAIN_ID, MANTA_CHAIN_ID,
        MODE_CHAIN_ID, SONEIUM_CHAIN_ID, UNICHAIN_CHAIN_ID, WORLD_CHAIN_ID,
    };
    use leafage_evm_types::U256;

    fn block(number: u64, timestamp: u64) -> BlockEnv {
        BlockEnv {
            number: U256::from(number),
            timestamp: U256::from(timestamp),
            ..Default::default()
        }
    }

    fn op_mainnet_cfg() -> CfgEnv<OpSpecId> {
        let mut cfg = CfgEnv::new_with_spec(OpSpecId::KARST);
        cfg.chain_id = OP_MAINNET_CHAIN_ID;
        cfg.disable_base_fee = true;
        cfg.tx_gas_limit_cap = Some(50_000_000);
        cfg
    }

    fn api(cfg: CfgEnv<OpSpecId>) -> OpApiImpl<()> {
        OpApiImpl::<()>::new(
            (),
            cfg,
            None,
            None,
            None,
            None,
            false,
            false,
            String::new(),
            0,
            None,
            None,
            None,
        )
    }

    #[test]
    fn mainnet_spec_and_gas_params_follow_the_block() {
        let cfg = op_mainnet_cfg();
        for (block_env, spec) in [
            (block(105_235_062, 1_686_068_901), OpSpecId::BEDROCK),
            (block(105_235_063, 1_686_068_903), OpSpecId::REGOLITH),
            (block(114_696_812, 1_704_992_401), OpSpecId::CANYON),
            (block(135_603_811, 1_746_806_399), OpSpecId::HOLOCENE),
            (block(135_603_812, 1_746_806_401), OpSpecId::ISTHMUS),
            (block(153_963_812, 1_783_526_401), OpSpecId::KARST),
        ] {
            let actual = block_cfg(&cfg, &block_env);
            assert_eq!(actual.spec, spec);
            assert_eq!(actual.gas_params, CfgEnv::new_with_spec(spec).gas_params);
            // Everything else is the configured cfg.
            assert_eq!(actual.chain_id, cfg.chain_id);
            assert_eq!(actual.tx_gas_limit_cap, cfg.tx_gas_limit_cap);
            assert!(actual.disable_base_fee);
        }
    }

    #[test]
    fn soneium_follows_its_own_forks() {
        let mut cfg = op_mainnet_cfg();
        cfg.chain_id = SONEIUM_CHAIN_ID;
        for (block_env, spec) in [
            // OP Mainnet reached Holocene at 1_736_445_601; Soneium was still on Granite.
            (block(1_932_625, 1_737_000_001), OpSpecId::GRANITE),
            (block(2_719_225, 1_738_573_201), OpSpecId::HOLOCENE),
            (block(6_835_824, 1_746_806_399), OpSpecId::HOLOCENE),
            (block(6_835_825, 1_746_806_401), OpSpecId::ISTHMUS),
            (block(25_195_825, 1_783_526_401), OpSpecId::KARST),
        ] {
            let actual = block_cfg(&cfg, &block_env);
            assert_eq!(actual.spec, spec);
            assert_eq!(actual.gas_params, CfgEnv::new_with_spec(spec).gas_params);
        }
    }

    #[test]
    fn unichain_follows_its_own_forks() {
        let mut cfg = op_mainnet_cfg();
        cfg.chain_id = UNICHAIN_CHAIN_ID;
        for (block_env, spec) in [
            (block(5_697_241, 1_736_445_600), OpSpecId::GRANITE),
            // Soneium was still on Granite at this timestamp.
            (block(6_251_642, 1_737_000_001), OpSpecId::HOLOCENE),
            (block(16_058_041, 1_746_806_400), OpSpecId::HOLOCENE),
            (block(16_058_042, 1_746_806_401), OpSpecId::ISTHMUS),
            (block(52_778_042, 1_783_526_401), OpSpecId::KARST),
        ] {
            let actual = block_cfg(&cfg, &block_env);
            assert_eq!(actual.spec, spec);
            assert_eq!(actual.gas_params, CfgEnv::new_with_spec(spec).gas_params);
        }
    }

    /// Cyber and Fraxtal have not reached Karst: their head runs an older fork, and
    /// the EIP-7825 estimate cap does not apply there.
    #[test]
    fn chains_behind_karst_run_their_own_head_fork() {
        let head = block(40_686_578, 1_794_801_723);
        for (chain_id, spec) in [
            (CYBER_CHAIN_ID, OpSpecId::HOLOCENE),
            (FRAXTAL_CHAIN_ID, OpSpecId::ISTHMUS),
        ] {
            let mut cfg = op_mainnet_cfg();
            cfg.chain_id = chain_id;
            let actual = block_cfg(&cfg, &head);
            assert_eq!(actual.spec, spec, "chain {chain_id}");
            assert_eq!(actual.gas_params, CfgEnv::new_with_spec(spec).gas_params);
            assert_eq!(
                api(cfg).consensus_tx_gas_limit_cap(SpecId::OSAKA, &head),
                u64::MAX,
                "chain {chain_id}"
            );
        }
        // Fraxtal before Ecotone runs Canyon.
        let mut cfg = op_mainnet_cfg();
        cfg.chain_id = FRAXTAL_CHAIN_ID;
        let canyon = block_cfg(&cfg, &block(5_099_244, 1_717_009_199));
        assert_eq!(canyon.spec, OpSpecId::CANYON);
        assert_eq!(
            canyon.gas_params,
            CfgEnv::new_with_spec(OpSpecId::CANYON).gas_params
        );
    }

    /// Mode follows OP Mainnet's timestamps; BOB and World Chain reached Jovian and
    /// Karst later, and the EIP-7825 estimate cap starts with each chain's own Karst.
    #[test]
    fn mode_bob_world_follow_their_own_forks() {
        let cap = revm::primitives::eip7825::TX_GAS_LIMIT_CAP;
        for (timestamp, expected) in [
            // After OP Mainnet's Jovian, before BOB's and World Chain's.
            (
                1_770_000_001,
                [
                    (MODE_CHAIN_ID, OpSpecId::JOVIAN, u64::MAX),
                    (BOB_CHAIN_ID, OpSpecId::ISTHMUS, u64::MAX),
                    (WORLD_CHAIN_ID, OpSpecId::ISTHMUS, u64::MAX),
                ],
            ),
            // After OP Mainnet's Karst, before World Chain's (09-21) and BOB's (09-23).
            (
                1_785_000_001,
                [
                    (MODE_CHAIN_ID, OpSpecId::KARST, cap),
                    (BOB_CHAIN_ID, OpSpecId::JOVIAN, u64::MAX),
                    (WORLD_CHAIN_ID, OpSpecId::JOVIAN, u64::MAX),
                ],
            ),
            // World Chain is on Karst, BOB two days short of it.
            (
                1_790_000_001,
                [
                    (MODE_CHAIN_ID, OpSpecId::KARST, cap),
                    (BOB_CHAIN_ID, OpSpecId::JOVIAN, u64::MAX),
                    (WORLD_CHAIN_ID, OpSpecId::KARST, cap),
                ],
            ),
        ] {
            let block_env = block(30_000_000, timestamp);
            for (chain_id, spec, estimate_cap) in expected {
                let mut cfg = op_mainnet_cfg();
                cfg.chain_id = chain_id;
                let actual = block_cfg(&cfg, &block_env);
                assert_eq!(actual.spec, spec, "chain {chain_id} at {timestamp}");
                assert_eq!(actual.gas_params, CfgEnv::new_with_spec(spec).gas_params);
                assert_eq!(
                    api(cfg).consensus_tx_gas_limit_cap(SpecId::OSAKA, &block_env),
                    estimate_cap,
                    "chain {chain_id} at {timestamp}"
                );
            }
        }
        // Mode starts on Regolith.
        let mut cfg = op_mainnet_cfg();
        cfg.chain_id = MODE_CHAIN_ID;
        assert_eq!(
            block_cfg(&cfg, &block(2_412_408, 1_704_992_399)).spec,
            OpSpecId::REGOLITH
        );
    }

    /// Manta Pacific stops at Isthmus, so its head has no EIP-7825 estimate cap; Katana
    /// reaches Jovian and Karst at its own times.
    #[test]
    fn manta_katana_follow_their_own_forks() {
        let cap = revm::primitives::eip7825::TX_GAS_LIMIT_CAP;
        for (chain_id, block_env, spec, estimate_cap) in [
            (
                MANTA_CHAIN_ID,
                block(3_255_924, 1_726_783_199),
                OpSpecId::REGOLITH,
                u64::MAX,
            ),
            (
                MANTA_CHAIN_ID,
                block(3_255_925, 1_726_783_209),
                OpSpecId::ECOTONE,
                u64::MAX,
            ),
            (
                MANTA_CHAIN_ID,
                block(9_535_765, 1_789_581_609),
                OpSpecId::ISTHMUS,
                u64::MAX,
            ),
            // 2026-11-16, after OP Mainnet's Karst.
            (
                MANTA_CHAIN_ID,
                block(10_059_354, 1_794_817_499),
                OpSpecId::ISTHMUS,
                u64::MAX,
            ),
            (
                KATANA_CHAIN_ID,
                block(1, 1_746_742_812),
                OpSpecId::HOLOCENE,
                u64::MAX,
            ),
            (
                KATANA_CHAIN_ID,
                block(26_323_790, 1_773_066_601),
                OpSpecId::JOVIAN,
                u64::MAX,
            ),
            // After OP Mainnet's Karst, before Katana's.
            (
                KATANA_CHAIN_ID,
                block(43_436_389, 1_790_179_200),
                OpSpecId::JOVIAN,
                u64::MAX,
            ),
            (
                KATANA_CHAIN_ID,
                block(43_436_390, 1_790_179_201),
                OpSpecId::KARST,
                cap,
            ),
        ] {
            let mut cfg = op_mainnet_cfg();
            cfg.chain_id = chain_id;
            let actual = block_cfg(&cfg, &block_env);
            assert_eq!(
                actual.spec, spec,
                "chain {chain_id} block {}",
                block_env.number
            );
            assert_eq!(actual.gas_params, CfgEnv::new_with_spec(spec).gas_params);
            assert_eq!(
                api(cfg).consensus_tx_gas_limit_cap(SpecId::OSAKA, &block_env),
                estimate_cap,
                "chain {chain_id} block {}",
                block_env.number
            );
        }
    }

    #[test]
    fn other_chains_keep_the_configured_spec() {
        let mut cfg = CfgEnv::new_with_spec(OpSpecId::JOVIAN);
        cfg.chain_id = 4153;
        let actual = block_cfg(&cfg, &block(1, 0));
        assert_eq!(actual.spec, OpSpecId::JOVIAN);
        assert_eq!(actual.gas_params, cfg.gas_params);
    }

    #[test]
    fn estimate_cap_follows_the_block() {
        let op = api(op_mainnet_cfg());
        let jovian = block(153_963_811, 1_783_526_399);
        let karst = block(153_963_812, 1_783_526_401);
        // The configured spec (passed by debank estimateGas) no longer decides.
        assert_eq!(
            op.consensus_tx_gas_limit_cap(SpecId::OSAKA, &jovian),
            u64::MAX
        );
        assert_eq!(
            op.consensus_tx_gas_limit_cap(SpecId::OSAKA, &karst),
            revm::primitives::eip7825::TX_GAS_LIMIT_CAP
        );

        // Other OP-stack chains keep the configured (Karst) cap at every height.
        let mut other = op_mainnet_cfg();
        other.chain_id = 8453;
        assert_eq!(
            api(other).consensus_tx_gas_limit_cap(SpecId::OSAKA, &jovian),
            revm::primitives::eip7825::TX_GAS_LIMIT_CAP
        );
    }
}
