use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::core::{
    ApiCore, EvmExecutor, GasFeeHandler, GetHaltReason, GetTransactionError, ToJsonRpcError, TxSetter,
};
use crate::api_impl::mainnet::evm::{create_main_evm_from_state, create_mainnet_txn_env};
use crate::api_impl::ApiImpl;
use crate::error::{internal_rpc_err, rpc_error_with_code};
use alloy::consensus::BlockHeader;
use alloy::eips::eip2935::HISTORY_STORAGE_ADDRESS;
use jsonrpsee::core::RpcResult;
use leafage_evm_types::{CallRequest, DebankErrorCode, MainnetSpecId, H256};
use revm::context::result::{EVMError, HaltReason, InvalidTransaction};
use revm::context::{result::ExecutionResult, BlockEnv, TxEnv};
use revm::inspector::NoOpInspector;
use revm::primitives::hardfork::SpecId;
use revm::ExecuteEvm;
use revm::InspectCommitEvm;
use revm::SystemCallEvm;
use revm::{DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

type MainnetApiImpl<DB> = ApiImpl<DB, MainnetSpecId, NoneEvmCustomConfig>;

impl<DB> MainnetApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    fn apply_blockhashes_contract_call<StateDB>(
        &self,
        parent_block_hash: H256,
        block_env: &BlockEnv,
        state: &mut StateDB,
    ) -> RpcResult<()>
    where
        StateDB: DatabaseCommit + DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        if !self.evm_cfg.cfg.spec.is_enabled_in(SpecId::PRAGUE) {
            return Ok(());
        }

        if block_env.number.is_zero() {
            return Ok(());
        }

        let result = {
            let mut evm = create_main_evm_from_state(
                block_env.clone(),
                self.evm_cfg.cfg.clone(),
                &*state,
                NoOpInspector {},
            );

            evm.system_call(HISTORY_STORAGE_ADDRESS, parent_block_hash.0.into())
                .map_err(|e| {
                    internal_rpc_err(format!("EIP-2935 blockhashes contract call failed: {e}"))
                })?
        };

        state.commit(result.state);
        Ok(())
    }
}

impl<DB> GasFeeHandler for MainnetApiImpl<DB> where DB: Sync + Send + 'static { type Tx = TxEnv; }

impl<DB> EvmExecutor for MainnetApiImpl<DB>
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
        let mut evm = create_main_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            NoOpInspector {},
        );

        let res = evm.transact(tx).map(|res| res.result.into());
        res
    }

    fn apply_pre_execution_changes<StateDB>(
        &self,
        header: impl BlockHeader,
        block_env: &BlockEnv,
        state: &mut StateDB,
    ) -> RpcResult<()>
    where
        StateDB: DatabaseCommit + DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        self.apply_blockhashes_contract_call(header.parent_hash(), block_env, state)
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
        let mut evm = create_main_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            &mut inspector,
        );

        let res = evm
            .inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)));

        res
    }
}

impl ToJsonRpcError for InvalidTransaction {
    fn to_rpc_error(&self) -> jsonrpsee::types::ErrorObjectOwned {
        match self {
            InvalidTransaction::LackOfFundForMaxFee { .. } => rpc_error_with_code(
                DebankErrorCode::BalanceExhausted as i32,
                "Insufficient funds".to_string(),
            ),
            InvalidTransaction::CallerGasLimitMoreThanBlock => rpc_error_with_code(
                DebankErrorCode::InvalidParams as i32,
                "Caller gas limit more than block".to_string(),
            ),
            InvalidTransaction::CallGasCostMoreThanGasLimit { .. } => rpc_error_with_code(
                DebankErrorCode::GasExhausted as i32,
                "Invalid gas limit".to_string(),
            ),
            InvalidTransaction::NonceOverflowInTransaction
            | InvalidTransaction::NonceTooHigh { .. }
            | InvalidTransaction::NonceTooLow { .. } => rpc_error_with_code(
                DebankErrorCode::NonceError as i32,
                "Invalid nonce".to_string(),
            ),
            e => rpc_error_with_code(DebankErrorCode::EvmFailed as i32, e.to_string()),
        }
    }
}

impl GetTransactionError for InvalidTransaction {
    fn get_transaction_error(&self) -> Option<InvalidTransaction> {
        Some(self.clone())
    }
}

impl TxSetter for TxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.gas_limit = gas_limit;
    }
}

impl GetHaltReason for HaltReason {
    fn get_halt_reason(&self) -> Option<HaltReason> {
        Some(self.clone())
    }
}

impl<DB> ApiCore for MainnetApiImpl<DB> where DB: Sync + Send + 'static {}
