use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::core::GetHaltReason;
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::{ApiCore, ApiImpl, EvmExecutor, GasFeeHandler};
use crate::error::invalid_params_rpc_err;
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::monad::{MonadEvm, MonadHaltReason, MonadHardfork, TFM_MAX_TX_GAS_LIMIT};
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest, CfgEnv};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::TxEnv;
use revm::database::WrapDatabaseRef;
use revm::inspector::NoOpInspector;
use revm::primitives::hardfork::SpecId as EthSpecId;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type MonadApiImpl<DB> = ApiImpl<DB, MonadHardfork, NoneEvmCustomConfig>;

impl<DB> MonadApiImpl<DB> {
    /// Monad revisions activate by block timestamp
    /// (`MonadMainnet::get_monad_revision`).
    fn cfg_env(&self, block_env: &BlockEnv) -> CfgEnv<MonadHardfork> {
        let timestamp = block_env.timestamp.saturating_to();
        let hardfork = MonadHardfork::active_at_timestamp(timestamp);
        let mut cfg = self.evm_cfg.cfg.clone();
        hardfork.apply_cfg(&mut cfg);
        cfg
    }

    fn evm_env(&self, block_env: &BlockEnv) -> EvmEnv<MonadHardfork> {
        EvmEnv::new(self.cfg_env(block_env), block_env.clone())
    }
}

impl<DB> GasFeeHandler for MonadApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;

    /// Monad does not enforce EIP-7825; consensus rejects transactions above
    /// the TFM gas limit instead.
    fn consensus_tx_gas_limit_cap(&self, _spec: EthSpecId) -> u64 {
        TFM_MAX_TX_GAS_LIMIT
    }
}

impl<DB> EvmExecutor for MonadApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = MonadHaltReason;

    /// `validate_transaction.cpp`: `eip_4844_active()` is always false on
    /// Monad, blob transactions are `TypeNotSupported`.
    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        let has_blob_hashes = request
            .inner
            .blob_versioned_hashes
            .as_ref()
            .is_some_and(|hashes| !hashes.is_empty());
        if has_blob_hashes
            || request.inner.sidecar.is_some()
            || request.inner.max_fee_per_blob_gas.is_some()
        {
            return Err(invalid_params_rpc_err(
                "EIP-4844 blob transactions are not supported on Monad",
            ));
        }
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
        let mut evm = MonadEvm::new(evm_env, wrap_database_ref, NoOpInspector {});
        evm.transact(tx).map(|res| res.result)
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
        let mut evm = MonadEvm::new(evm_env, wrap_database_ref, &mut inspector);
        evm.inspect_tx_commit(tx)
            .map(|res| (res, inspector_collect(inspector)))
    }
}

impl GetHaltReason for MonadHaltReason {
    fn get_halt_reason(&self) -> Option<HaltReason> {
        match self {
            MonadHaltReason::Base(halt) => Some(halt.clone()),
            // Gas estimation treats a reserve balance violation like any
            // other non-gas failure (the error is reported to the caller).
            MonadHaltReason::ReserveBalanceViolation => Some(HaltReason::OutOfFunds),
        }
    }
}

impl<DB> ApiCore for MonadApiImpl<DB> where DB: Sync + Send + 'static {}
