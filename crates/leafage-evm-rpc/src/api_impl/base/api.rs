use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::base::evm::{create_base_evm_from_state, create_base_txn_env};
use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler};
use crate::api_impl::ApiImpl;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::base::BaseHardfork;
use leafage_evm_types::{BlockInfo, CallRequest};
use op_revm::{OpHaltReason, OpTransaction, OpTransactionError};
use revm::context::result::EVMError;
use revm::context::{result::ExecutionResult, BlockEnv, TxEnv};
use revm::inspector::NoOpInspector;
use revm::ExecuteEvm;
use revm::InspectCommitEvm;
use revm::{DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

type BaseApiImpl<DB> = ApiImpl<DB, BaseHardfork, NoneEvmCustomConfig>;

// Base reuses the op transaction/halt-reason types, so the `ToJsonRpcError`,
// `GetTransactionError`, `TxSetter`, and `GetHaltReason` impls for the `Op*`
// types defined in the `op` module apply here too (orphan rules forbid
// re-implementing them).

impl<DB> ApiCore for BaseApiImpl<DB> where DB: Sync + Send + 'static {}

impl<DB> GasFeeHandler for BaseApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = OpTransaction<TxEnv>;
}

impl<DB> EvmExecutor for BaseApiImpl<DB>
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
        create_base_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)
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
        let mut evm = create_base_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            NoOpInspector {},
        );
        evm.transact(tx).map(|res| res.result.into())
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
        let mut evm = create_base_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            &mut inspector,
        );
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}
