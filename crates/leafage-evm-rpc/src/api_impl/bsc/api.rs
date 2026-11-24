use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::core::{ApiCore, CallExecutor, EvmExecutor, GasFeeHandler, TxSetter};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::bsc::mainnet::{bsc_mainnet_spec_at, BSC_MAINNET_CHAIN_ID};
use leafage_evm_chains::bsc::{header_milli_timestamp, BscEvm, BscHardfork, BscTxEnv};
use leafage_evm_types::CfgEnv;
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::database::WrapDatabaseRef;
use revm::inspector::NoOpInspector;
use revm::primitives::hardfork::SpecId;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type BscApiImpl<DB> = ApiImpl<DB, BscHardfork, NoneEvmCustomConfig>;

/// The configured cfg with the hardfork of the executing block. On BSC mainnet the
/// spec and its gas params follow the fork schedule (as `trace_debankBlock` replay does),
/// so a new fork takes effect at its activation block without a redeploy; any other chain
/// id keeps the configured spec. A block of the configured fork keeps the configured gas
/// params.
fn block_cfg(cfg: &CfgEnv<BscHardfork>, block_env: &BlockEnv) -> CfgEnv<BscHardfork> {
    let mut cfg = cfg.clone();
    if cfg.chain_id == BSC_MAINNET_CHAIN_ID {
        let spec = bsc_mainnet_spec_at(
            block_env.number.saturating_to(),
            block_env.timestamp.saturating_to(),
        );
        if spec != cfg.spec {
            cfg.set_spec_and_mainnet_gas_params(spec);
        }
    }
    cfg
}

impl<DB> GasFeeHandler for BscApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = BscTxEnv;

    /// EIP-7825 follows the executing block's fork, not the configured spec.
    fn consensus_tx_gas_limit_cap(&self, _spec: SpecId, block_env: &BlockEnv) -> u64 {
        let spec: SpecId = block_cfg(&self.evm_cfg.cfg, block_env).spec.into();
        if spec.is_enabled_in(SpecId::OSAKA) {
            revm::primitives::eip7825::TX_GAS_LIMIT_CAP
        } else {
            u64::MAX
        }
    }
}

impl<DB> EvmExecutor for BscApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = BscTxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        let txn_env = create_mainnet_txn_env(
            block_env,
            block_cfg(&self.evm_cfg.cfg, block_env),
            request,
            db,
            chain_id,
        )?;
        let mut tx = BscTxEnv::new(txn_env);
        tx.milli_timestamp = Some(header_milli_timestamp(
            block.header.timestamp,
            block.header.mix_hash,
        ));
        Ok(tx)
    }

    fn transact<StateDB: DatabaseRef + Debug>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB::Error: Sync + Send + 'static,
    {
        let evm_env = EvmEnv::new(block_cfg(&self.evm_cfg.cfg, block_env), block_env.clone());
        let wrap_database_ref = WrapDatabaseRef(state);
        let mut evm = BscEvm::new(evm_env, wrap_database_ref, NoOpInspector {}, false);
        let res = evm.transact(tx).map(|res| res.result.into());
        res
    }

    /// Keeps one EVM for the whole run, so its precompile map, instruction
    /// table and frame stack are built once instead of per call.
    fn with_call_executor<StateDB, R>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        f: impl FnOnce(&mut CallExecutor<'_, Self, StateDB>) -> R,
    ) -> R
    where
        StateDB: DatabaseRef + Debug + Clone,
        StateDB::Error: Sync + Send + 'static,
    {
        let cfg = block_cfg(&self.evm_cfg.cfg, block_env);
        let evm_env = EvmEnv::new(cfg, block_env.clone());
        let mut evm = BscEvm::new(evm_env, WrapDatabaseRef(state), NoOpInspector {}, false);
        f(&mut |tx| {
            // `transact` finalizes the journal on success and on error, so the
            // next call starts from the unchanged state.
            evm.transact(tx).map(|res| res.result)
        })
    }

    fn inspect_tx_commit<StateDB, R, F>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseCommit + DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
        F: FnOnce(TracingInspector) -> R,
    {
        let evm_env = EvmEnv::new(block_cfg(&self.evm_cfg.cfg, block_env), block_env.clone());
        let wrap_database_ref = WrapDatabaseRef(state);
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = BscEvm::new(evm_env, wrap_database_ref, &mut inspector, true);
        let res = evm
            .inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)));

        res
    }
}

impl TxSetter for BscTxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.gas_limit = gas_limit;
    }
}

impl<DB> ApiCore for BscApiImpl<DB> where DB: Sync + Send + 'static {}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_types::U256;

    fn block(number: u64, timestamp: u64) -> BlockEnv {
        BlockEnv {
            number: U256::from(number),
            timestamp: U256::from(timestamp),
            ..Default::default()
        }
    }

    /// A multicall's calls run on one EVM give the same results as a fresh
    /// EVM per call.
    #[test]
    fn call_executor_matches_per_call_transact() {
        use crate::api_impl::core::call_executor_test::assert_matches_transact;

        let api = BscApiImpl::<()>::new(
            (),
            CfgEnv::<BscHardfork>::default(),
            None,
            None,
            None,
            None,
            false,
            false,
            String::new(),
            100,
            None,
            None,
            None,
        );
        let block_env = BlockEnv {
            basefee: 3,
            beneficiary: leafage_evm_types::Address::repeat_byte(0xbb),
            ..Default::default()
        };
        assert_matches_transact(&api, &block_env, BscTxEnv::new);
    }

    #[test]
    fn mainnet_spec_follows_the_block() {
        let mut cfg = CfgEnv::<BscHardfork>::default();
        cfg.chain_id = BSC_MAINNET_CHAIN_ID;
        // The default spec is Maxwell; the block decides.
        assert_eq!(cfg.spec, BscHardfork::Maxwell);
        assert_eq!(
            block_cfg(&cfg, &block(100_000_000, 1_796_697_000)).spec,
            BscHardfork::Jenner
        );
        assert_eq!(
            block_cfg(&cfg, &block(70_000_000, 1_787_625_000)).spec,
            BscHardfork::Pasteur
        );
        assert_eq!(
            block_cfg(&cfg, &block(60_000_000, 1_777_343_399)).spec,
            BscHardfork::Fermi
        );
        assert_eq!(block_cfg(&cfg, &block(1, 0)).spec, BscHardfork::Niels);
    }

    /// The gas table is the block's fork's, not the configured fork's: Niels runs
    /// Istanbul rules, without the cold access costs of the default Maxwell (Prague).
    #[test]
    fn mainnet_gas_params_follow_the_block() {
        use revm::context_interface::cfg::GasParams;

        let mut cfg = CfgEnv::<BscHardfork>::default();
        cfg.chain_id = BSC_MAINNET_CHAIN_ID;
        let niels = block_cfg(&cfg, &block(1, 0));
        assert_eq!(niels.spec, BscHardfork::Niels);
        assert_eq!(
            niels.gas_params.table(),
            GasParams::new_spec(SpecId::ISTANBUL).table()
        );
        assert_ne!(niels.gas_params.table(), cfg.gas_params.table());

        // A gas table set on the configured fork stays for a block of that fork.
        let mut cfg = CfgEnv::new_with_spec(BscHardfork::Fermi);
        cfg.chain_id = BSC_MAINNET_CHAIN_ID;
        cfg.gas_params = GasParams::new_spec(SpecId::ISTANBUL);
        let fermi = block_cfg(&cfg, &block(60_000_000, 1_777_343_399));
        assert_eq!(fermi.spec, BscHardfork::Fermi);
        assert_eq!(fermi.gas_params.table(), cfg.gas_params.table());
    }

    #[test]
    fn other_chains_keep_the_configured_spec() {
        let mut cfg = CfgEnv::new_with_spec(BscHardfork::Mendel);
        cfg.chain_id = 97;
        assert_eq!(
            block_cfg(&cfg, &block(70_000_000, 1_787_625_000)).spec,
            BscHardfork::Mendel
        );
    }

    /// End to end: a contract using Osaka's CLZ, run through the live executor with the
    /// default (Maxwell) config, follows the block's fork.
    #[test]
    fn executor_runs_the_block_fork() {
        use alloy::primitives::{Address, Bytes};
        use revm::{bytecode::Bytecode, database::InMemoryDB, state::AccountInfo};

        let mut cfg = CfgEnv::<BscHardfork>::default();
        cfg.chain_id = BSC_MAINNET_CHAIN_ID;
        cfg.disable_base_fee = true;
        let api = BscApiImpl::<()>::new(
            (),
            cfg,
            None,
            None,
            None,
            None,
            false,
            false,
            String::new(),
            100,
            None,
            None,
            None,
        );
        let caller = Address::repeat_byte(0x11);
        let target = Address::repeat_byte(0x22);
        let mut state = InMemoryDB::default();
        // PUSH1 1, CLZ, PUSH0, MSTORE, PUSH1 32, PUSH0, RETURN
        let code = Bytecode::new_raw(Bytes::from_static(&[
            0x60, 0x01, 0x1e, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
        ]));
        state.insert_account_info(
            target,
            AccountInfo {
                code_hash: code.hash_slow(),
                code: Some(code),
                nonce: 1,
                ..Default::default()
            },
        );
        let tx = BscTxEnv::new(
            revm::context::TxEnv::builder()
                .caller(caller)
                .to(target)
                .gas_limit(100_000)
                .chain_id(Some(BSC_MAINNET_CHAIN_ID))
                .build()
                .unwrap(),
        );

        let pasteur = api
            .transact(&block(70_000_000, 1_787_625_000), &state, tx.clone())
            .unwrap();
        assert_eq!(
            pasteur.output().unwrap().as_ref(),
            U256::from(255).to_be_bytes::<32>(),
            "{pasteur:?}"
        );
        let fermi = api
            .transact(&block(60_000_000, 1_777_343_399), &state, tx)
            .unwrap();
        assert!(matches!(fermi, ExecutionResult::Halt { .. }), "{fermi:?}");
    }
}
