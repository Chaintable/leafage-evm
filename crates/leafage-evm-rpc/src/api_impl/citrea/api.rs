use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::citrea::evm::{create_citrea_evm_from_state, create_citrea_handler_evm};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::{ApiCore, ApiImpl, EvmExecutor, GasFeeHandler};
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::citrea::CitreaHardfork;
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest, U256};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::TxEnv;
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

type CitreaApiImpl<DB> = ApiImpl<DB, CitreaHardfork, NoneEvmCustomConfig>;

impl<DB> GasFeeHandler for CitreaApiImpl<DB> where DB: Sync + Send + 'static {}

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

    fn transact<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let mut evm = create_citrea_evm_from_state(
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
        let mut evm = create_citrea_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            &mut inspector,
        );
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }

    fn estimate_l1_gas_overhead<StateDB: DatabaseRef>(
        &self,
        block: &BlockInfo,
        _gas_used: u64,
        tx: Self::Tx,
        db: StateDB,
        block_env: &BlockEnv,
    ) -> RpcResult<u64>
    where
        StateDB: std::fmt::Debug,
        StateDB::Error: std::fmt::Debug,
    {
        let l1_fee_rate = extract_l1_fee_rate(block);
        if l1_fee_rate == 0 {
            return Ok(0);
        }

        let mut evm = create_citrea_handler_evm(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            db,
            NoOpInspector {},
            l1_fee_rate,
        );

        let result = evm.transact(tx).map_err(|e| {
            crate::error::rpc_error_with_code(
                leafage_evm_types::DebankErrorCode::EvmFailed as i32,
                format!("L1 fee estimation failed: {e:?}"),
            )
        })?;

        if !result.result.is_success() {
            return Ok(0);
        }

        let tx_info = evm.tx_info();
        let l1_fee = tx_info.l1_fee;
        if l1_fee.is_zero() {
            return Ok(0);
        }

        let base_fee = U256::from(block_env.basefee);
        let effective_price = base_fee.max(U256::from(1));

        let overhead = l1_fee.checked_div(effective_price).unwrap_or(U256::ZERO);
        let overhead = overhead.saturating_add(U256::from(1));

        Ok(overhead.try_into().unwrap_or(u64::MAX))
    }
}

impl<DB> ApiCore for CitreaApiImpl<DB> where DB: Sync + Send + 'static {}

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
