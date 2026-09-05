use super::trace::{record_subcall_trace_completion, ArcSubcallTraceSidecar};
use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arc::{ArcChainConfig, ArcEvmFactory};
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest, MainnetSpecId};
use revm::{
    context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
    context::TxEnv,
    database::WrapDatabaseRef,
    inspector::NoOpInspector,
    DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type ArcApiImpl<DB> = ApiImpl<DB, MainnetSpecId, ArcChainConfig>;

impl<DB> ArcApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    fn arc_factory(&self) -> Result<ArcEvmFactory, String> {
        self.evm_cfg
            .custom_cfg
            .map(ArcEvmFactory::new)
            .ok_or_else(|| "Arc EVM chain configuration is missing".to_string())
    }
}

impl<DB> GasFeeHandler for ArcApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
}

impl<DB> EvmExecutor for ArcApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
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
        create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)
    }

    fn transact<StateDB>(
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
        let factory = self.arc_factory().map_err(EVMError::Custom)?;
        let env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let mut evm = factory
            .create(env, WrapDatabaseRef(state), NoOpInspector {})
            .map_err(|err| EVMError::Custom(err.to_string()))?;
        evm.transact(tx).map(|result| result.result)
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
        let factory = self.arc_factory().map_err(EVMError::Custom)?;
        let env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let mut inspectors = (
            TracingInspector::new(inspector_cfg),
            ArcSubcallTraceSidecar::new(),
        );
        let mut evm = factory
            .create(env, WrapDatabaseRef(state), &mut inspectors)
            .map_err(|err| EVMError::Custom(err.to_string()))?;
        evm.set_subcall_trace_completion_hook(record_subcall_trace_completion);
        let result = evm.inspect_tx_commit(tx)?;
        drop(evm);
        let (mut inspector, sidecar) = inspectors;
        sidecar.apply(&mut inspector).map_err(EVMError::Custom)?;
        Ok((result, inspector_collect(inspector)))
    }
}

impl<DB> ApiCore for ArcApiImpl<DB> where DB: Sync + Send + 'static {}
