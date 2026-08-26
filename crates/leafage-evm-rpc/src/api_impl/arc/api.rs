use crate::api_impl::core::{
    ApiCore, ArcEstimateGasPolicy, EstimateGasPolicy, EvmExecutor, GasFeeHandler,
};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::arc::{ArcChainConfig, ArcEvmFactory};
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest, MainnetSpecId};
use revm::{
    context::{
        result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
        TxEnv,
    },
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

    fn estimate_gas_policy(&self) -> EstimateGasPolicy {
        EstimateGasPolicy::Arc(ArcEstimateGasPolicy)
    }
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
        if self.evm_cfg.custom_cfg.is_none() {
            return Err(crate::error::internal_rpc_err(
                "Arc EVM chain configuration is missing",
            ));
        }
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

    fn transact_for_estimate<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
        hard_gas_cap: u64,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        let factory = self.arc_factory().map_err(EVMError::Custom)?;
        let mut cfg = self.evm_cfg.cfg.clone();
        cfg.disable_eip3607 = true;
        cfg.disable_base_fee = true;
        cfg.tx_gas_limit_cap = Some(hard_gas_cap);
        let env = EvmEnv::new(cfg, block_env.clone());
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
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = factory
            .create(env, WrapDatabaseRef(state), &mut inspector)
            .map_err(|err| EVMError::Custom(err.to_string()))?;
        evm.inspect_tx_commit(tx)
            .map(|result| (result, inspector_collect(inspector)))
    }
}

impl<DB> ApiCore for ArcApiImpl<DB> where DB: Sync + Send + 'static {}
