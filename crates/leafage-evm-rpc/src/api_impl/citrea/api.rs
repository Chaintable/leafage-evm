use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::{ApiCore, ApiImpl, EvmExecutor};
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::citrea::l1_fee::{BROTLI_COMPRESSION_PERCENTAGE, L1_FEE_OVERHEAD};
use leafage_evm_chains::citrea::{CitreaEvm, CitreaEvmConfig, CitreaHardfork};
use leafage_evm_types::{BlockEnv, CallRequest};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::TxEnv;
use revm::database::WrapDatabaseRef;
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type CitreaApiImpl<DB> = ApiImpl<DB, CitreaHardfork, CitreaEvmConfig>;

impl<DB> EvmExecutor for CitreaApiImpl<DB>
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
        create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)
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
        let mut evm = CitreaEvm::new(evm_env, wrap_database_ref, NoOpInspector {}, false);
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
        let mut evm = CitreaEvm::new(evm_env, wrap_database_ref, &mut inspector, true);
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }

    fn estimate_l1_overhead<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> u64
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        let l1_fee_rate = self
            .evm_cfg
            .custom_cfg
            .as_ref()
            .map(|cfg| cfg.l1_fee_rate)
            .unwrap_or(0);
        if l1_fee_rate == 0 {
            return 0;
        }
        let base_fee = block_env.basefee;
        if base_fee == 0 {
            return 0;
        }

        let evm_env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let wrap_database_ref = WrapDatabaseRef(state);
        let mut evm = CitreaEvm::new(evm_env, wrap_database_ref, NoOpInspector {}, false);

        let diff_size = match evm.transact_with_diff_size(tx) {
            Ok((_, diff_size)) => diff_size,
            Err(_) => return 0,
        };

        let compressed =
            (diff_size * BROTLI_COMPRESSION_PERCENTAGE / 100 + L1_FEE_OVERHEAD) as u128;
        let l1_fee = l1_fee_rate * compressed;
        let base_fee_u128 = base_fee as u128;
        ((l1_fee + base_fee_u128 - 1) / base_fee_u128) as u64
    }
}

impl<DB> ApiCore for CitreaApiImpl<DB> where DB: Sync + Send + 'static {}
