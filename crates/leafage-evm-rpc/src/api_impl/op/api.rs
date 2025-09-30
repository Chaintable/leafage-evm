use crate::api_impl::core::{
    ApiCore, EvmExecuter, GetHaltReason, GetTransactionError, ToJsonRpcError, TxSetter,
};
use crate::api_impl::op::evm::{create_op_evm_from_state, create_op_txn_env};
use crate::api_impl::ApiImpl;
use crate::error::rpc_error_with_code;
use jsonrpsee::core::RpcResult;
use leafage_evm_types::{CallRequest, DebankErrorCode, OpSpecId};
use op_revm::{OpHaltReason, OpTransaction, OpTransactionError};
use revm::context::result::{EVMError, HaltReason, InvalidTransaction};
use revm::context::{result::ExecutionResult, BlockEnv, TxEnv};
use revm::inspector::NoOpInspector;
use revm::ExecuteEvm;
use revm::InspectCommitEvm;
use revm::{DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

type OpApiImpl<DB> = ApiImpl<DB, OpSpecId>;

impl<DB> EvmExecuter for OpApiImpl<DB>
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
        create_op_txn_env(block_env, request, db, chain_id)
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
        let mut evm = create_op_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            NoOpInspector {},
        );

        let res = evm.transact(tx).map(|res| res.result.into());
        res
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
        let mut evm = create_op_evm_from_state(
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

impl ToJsonRpcError for OpTransactionError {
    fn to_rpc_error(&self) -> jsonrpsee::types::ErrorObjectOwned {
        match self {
            OpTransactionError::Base(invalid_tx) => invalid_tx.to_rpc_error(),
            e => rpc_error_with_code(DebankErrorCode::EvmFailed as i32, e.to_string()),
        }
    }
}

impl GetTransactionError for OpTransactionError {
    fn get_transaction_error(&self) -> Option<InvalidTransaction> {
        match self {
            OpTransactionError::Base(invalid_tx) => Some(invalid_tx.clone()),
            _ => None,
        }
    }
}

impl TxSetter for OpTransaction<TxEnv> {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.gas_limit = gas_limit;
    }
}

impl GetHaltReason for OpHaltReason {
    fn get_halt_reason(&self) -> Option<HaltReason> {
        match self {
            OpHaltReason::Base(halt) => Some(halt.clone()),
            _ => None,
        }
    }
}

impl<DB> ApiCore for OpApiImpl<DB> where DB: Sync + Send + 'static {}
