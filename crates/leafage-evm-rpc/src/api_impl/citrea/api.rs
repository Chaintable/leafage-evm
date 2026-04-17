use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::{ApiCore, ApiImpl, EvmExecutor};
use alloy_evm::EvmEnv;
use leafage_evm_chains::citrea::l1_fee::{
    BROTLI_COMPRESSION_PERCENTAGE, L1_FEE_OVERHEAD, SYSTEM_SIGNER,
};
use leafage_evm_chains::citrea::{CitreaEvm, CitreaHardfork};
use leafage_evm_storage::EvmStorageRead;
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::TxEnv;
use revm::database::{CacheDB, WrapDatabaseRef};
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type CitreaApiImpl<DB> = ApiImpl<DB, CitreaHardfork, NoneEvmCustomConfig>;

/// Extract L1 fee rate from block's extra fields.
fn extract_l1_fee_rate(block: &BlockInfo) -> u128 {
    let val = match block.other.get("l1FeeRate") {
        Some(v) => v,
        None => return 0,
    };
    if let Some(n) = val.as_u64() {
        return n as u128;
    }
    if let Some(s) = val.as_str() {
        if let Some(hex) = s.strip_prefix("0x") {
            return u128::from_str_radix(hex, 16).unwrap_or(0);
        }
        return s.parse::<u128>().unwrap_or(0);
    }
    0
}

impl<DB> EvmExecutor for CitreaApiImpl<DB>
where
    DB: Sync + Send + 'static + EvmStorageRead,
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
    ) -> jsonrpsee::core::RpcResult<Self::Tx> {
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
        let env = EvmEnv {
            block_env: block_env.clone(),
            cfg_env: self.evm_cfg.cfg.clone(),
        };
        let mut evm = CitreaEvm::new(env, WrapDatabaseRef(state), NoOpInspector {}, false);
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
        let env = EvmEnv {
            block_env: block_env.clone(),
            cfg_env: self.evm_cfg.cfg.clone(),
        };
        let mut evm = CitreaEvm::new(env, WrapDatabaseRef(state), &mut inspector, true);
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}

impl<DB> crate::api_impl::core::GasFeeHandler for CitreaApiImpl<DB>
where
    DB: Sync + Send + 'static + EvmStorageRead,
{
    type Tx = TxEnv;

    fn estimate_l1_overhead<StateDB: DatabaseRef>(
        &self,
        block: &BlockInfo,
        block_env: &BlockEnv,
        tx: Self::Tx,
        state: &StateDB,
    ) -> u64
    where
        StateDB::Error: Sync + Send + 'static,
        StateDB: Debug,
    {
        if tx.caller == SYSTEM_SIGNER {
            return 0;
        }

        let l1_fee_rate = extract_l1_fee_rate(block);
        if l1_fee_rate == 0 {
            return 0;
        }

        let mut cfg_env = self.evm_cfg.cfg.clone();
        cfg_env.disable_nonce_check = true;
        cfg_env.disable_balance_check = true;
        cfg_env.disable_base_fee = true;

        let env = EvmEnv {
            block_env: block_env.clone(),
            cfg_env,
        };
        let fresh_db = CacheDB::new(WrapDatabaseRef(state));
        let mut evm = CitreaEvm::new(env, fresh_db, NoOpInspector {}, false);

        let result = match evm.transact_with_diff_size(tx) {
            Ok(r) => r,
            Err(_) => return 0,
        };

        let (exec_result, diff_size) = result;
        if !exec_result.is_success() || diff_size == 0 {
            return 0;
        }

        let compressed = diff_size * BROTLI_COMPRESSION_PERCENTAGE / 100 + L1_FEE_OVERHEAD;
        let l1_fee = l1_fee_rate * compressed as u128;

        let base_fee = block_env.basefee as u128;
        let effective_price = base_fee.max(1);

        let overhead = (l1_fee + effective_price - 1) / effective_price;
        overhead.try_into().unwrap_or(u64::MAX)
    }
}

impl<DB> ApiCore for CitreaApiImpl<DB> where DB: Sync + Send + 'static + EvmStorageRead {}
