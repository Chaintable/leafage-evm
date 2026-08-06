use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler, TxSetter};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::bsc::{BscEvm, BscHardfork, BscTxEnv};
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::database::WrapDatabaseRef;
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type BscApiImpl<DB> = ApiImpl<DB, BscHardfork, NoneEvmCustomConfig>;

impl<DB> GasFeeHandler for BscApiImpl<DB> where DB: Sync + Send + 'static { type Tx = BscTxEnv; }

impl<DB> EvmExecutor for BscApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = BscTxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        let txn_env =
            create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)?;
        Ok(BscTxEnv::new(txn_env))
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
        let evm_env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let wrap_database_ref = WrapDatabaseRef(state);
        let mut evm = BscEvm::new(evm_env, wrap_database_ref, NoOpInspector {}, false);
        let res = evm.transact(tx).map(|res| res.result.into());
        res
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
        let evm_env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
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
