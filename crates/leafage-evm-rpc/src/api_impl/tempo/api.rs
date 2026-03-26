use crate::api_impl::core::{ApiCore, EvmExecutor, TxSetter};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::tempo::tx::TempoTxEnv;
use leafage_evm_chains::tempo::TempoEvm;
use leafage_evm_types::{BlockEnv, CallRequest, MainnetSpecId};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::database::WrapDatabaseRef;
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

/// Marker type to differentiate `TempoApiImpl` from `MainnetApiImpl`.
///
/// Both use `MainnetSpecId`, but Rust's type system requires distinct types
/// for separate `EvmExecutor` implementations.
#[derive(Debug, Clone)]
pub struct TempoEvmCustomConfig;

type TempoApiImpl<DB> = ApiImpl<DB, MainnetSpecId, TempoEvmCustomConfig>;

impl<DB> EvmExecutor for TempoApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TempoTxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        // Build standard TxEnv first.
        let base =
            create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)?;

        // TODO: Extract tempo_calls and nonce_key from the RPC request.
        // alloy's TransactionRequest does not have these fields natively.
        // When the Tempo RPC endpoint is wired with custom deserialization,
        // parse the batch calls here and populate TempoTxFields.
        Ok(TempoTxEnv {
            base,
            tempo_fields: None,
        })
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
        let mut evm = TempoEvm::new(evm_env, wrap_database_ref, NoOpInspector {}, false);
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
        let evm_env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let wrap_database_ref = WrapDatabaseRef(state);
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = TempoEvm::new(evm_env, wrap_database_ref, &mut inspector, true);
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}

impl TxSetter for TempoTxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.gas_limit = gas_limit;
    }
}

impl<DB> ApiCore for TempoApiImpl<DB> where DB: Sync + Send + 'static {}
