use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::{ApiCore, ApiImpl, EvmExecutor, GasFeeHandler};
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::polygon::{PolygonEvm, PolygonHardfork};
use leafage_evm_types::{BlockEnv, CallRequest, CfgEnv};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::TxEnv;
use revm::database::WrapDatabaseRef;
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type PolygonApiImpl<DB> = ApiImpl<DB, PolygonHardfork, NoneEvmCustomConfig>;

impl<DB> PolygonApiImpl<DB> {
    fn cfg_env(&self, block_env: &BlockEnv) -> CfgEnv<PolygonHardfork> {
        let block_number = block_env.number.saturating_to();
        let hardfork = PolygonHardfork::active_at_block(block_number);
        let mut cfg = self.evm_cfg.cfg.clone();
        hardfork.apply_cfg(&mut cfg);
        cfg
    }

    fn evm_env(&self, block_env: &BlockEnv) -> EvmEnv<PolygonHardfork> {
        EvmEnv::new(self.cfg_env(block_env), block_env.clone())
    }
}

impl<DB> GasFeeHandler for PolygonApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
}

impl<DB> EvmExecutor for PolygonApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        create_mainnet_txn_env(block_env, self.cfg_env(block_env), request, db, chain_id)
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
        let evm_env = self.evm_env(block_env);
        let wrap_database_ref = WrapDatabaseRef(state);
        let mut evm = PolygonEvm::new(evm_env, wrap_database_ref, NoOpInspector {});
        evm.transact(tx).map(|res| res.result.into())
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
        let evm_env = self.evm_env(block_env);
        let wrap_database_ref = WrapDatabaseRef(state);
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = PolygonEvm::new(evm_env, wrap_database_ref, &mut inspector);
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}

impl<DB> ApiCore for PolygonApiImpl<DB> where DB: Sync + Send + 'static {}
