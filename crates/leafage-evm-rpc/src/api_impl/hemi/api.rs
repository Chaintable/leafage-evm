use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler};
use crate::api_impl::hemi::evm::{create_hemi_evm_from_state, create_hemi_txn_env};
use crate::api_impl::ApiImpl;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::hemi::HemiHardfork;
use leafage_evm_types::{BlockEnv, CallRequest};
use op_revm::{OpHaltReason, OpTransaction, OpTransactionError};
use revm::context::result::{EVMError, ExecutionResult};
use revm::context::TxEnv;
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type HemiApiImpl<DB> = ApiImpl<DB, HemiHardfork, NoneEvmCustomConfig>;

impl<DB> GasFeeHandler for HemiApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = OpTransaction<TxEnv>;
}

impl<DB> EvmExecutor for HemiApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = OpTransaction<TxEnv>;
    type TransactionError = OpTransactionError;
    type EvmHaltReason = OpHaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        create_hemi_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)
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
        let mut evm = create_hemi_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            NoOpInspector {},
        );
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
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = create_hemi_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            &mut inspector,
        );
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}

impl<DB> ApiCore for HemiApiImpl<DB> where DB: Sync + Send + 'static {}
