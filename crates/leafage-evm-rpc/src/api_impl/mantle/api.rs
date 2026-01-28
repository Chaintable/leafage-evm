use crate::api_impl::core::{ApiCore, EvmExecutor, TxSetter};
use crate::api_impl::mantle::evm::{create_mantle_evm_from_state, create_mantle_txn_env};
use crate::api_impl::ApiImpl;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::mantle::{GAS_ORACLE_ADDR, MantleHardfork, TOKEN_RATIO_SLOT};
use leafage_evm_types::CallRequest;
use op_revm::transaction::OpTxTr;
use op_revm::{OpHaltReason, OpTransaction, OpTransactionError};
use revm::context::result::EVMError;
use revm::context::{result::ExecutionResult, BlockEnv, TxEnv};
use revm::inspector::NoOpInspector;
use revm::ExecuteEvm;
use revm::InspectCommitEvm;
use revm::{DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use crate::api_impl::api_impl::NoneEvmCustomConfig;

type MantleApiImpl<DB> = ApiImpl<DB, MantleHardfork, NoneEvmCustomConfig>;

fn get_token_ratio<DB: DatabaseRef>(db: &DB) -> u64 {
    match db.storage_ref(GAS_ORACLE_ADDR, TOKEN_RATIO_SLOT) {
        Ok(storage_value) => storage_value.to::<u64>(),
        Err(_) => 1,
    }
}

impl<DB> EvmExecutor for MantleApiImpl<DB>
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
        create_mantle_txn_env(block_env, request, db, chain_id)
    }

    fn transact<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        mut tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let token_ratio = get_token_ratio(&state);

        let should_apply_ratio = token_ratio > 1
            && !tx.is_deposit()
            && !tx.is_system_transaction();

        if should_apply_ratio {
            let original_gas_limit = tx.base.gas_limit;
            tx.set_gas_limit(original_gas_limit / token_ratio);
        }

        let mut evm = create_mantle_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            NoOpInspector {},
        );

        let result = evm.transact(tx)?;

        let final_result = if should_apply_ratio {
            match result.result {
                ExecutionResult::Success { gas_used, gas_refunded, output, logs, reason } => {
                    ExecutionResult::Success {
                        gas_used: gas_used * token_ratio,
                        gas_refunded: gas_refunded * token_ratio,
                        output,
                        logs,
                        reason,
                    }
                }
                ExecutionResult::Revert { gas_used, output } => {
                    ExecutionResult::Revert {
                        gas_used: gas_used * token_ratio,
                        output,
                    }
                }
                ExecutionResult::Halt { reason, gas_used } => {
                    ExecutionResult::Halt {
                        reason,
                        gas_used: gas_used * token_ratio,
                    }
                }
            }
        } else {
            result.result
        };

        Ok(final_result.into())
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
        mut tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let token_ratio = get_token_ratio(&state);

        let should_apply_ratio = token_ratio > 1
            && !tx.is_deposit()
            && !tx.is_system_transaction();

        if should_apply_ratio {
            let original_gas_limit = tx.base.gas_limit;
            tx.set_gas_limit(original_gas_limit / token_ratio);
        }

        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = create_mantle_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            &mut inspector,
        );

        let result = evm.inspect_tx_commit(tx)?;

        let final_result = if should_apply_ratio {
            match result {
                ExecutionResult::Success { gas_used, gas_refunded, output, logs, reason } => {
                    ExecutionResult::Success {
                        gas_used: gas_used * token_ratio,
                        gas_refunded: gas_refunded * token_ratio,
                        output,
                        logs,
                        reason,
                    }
                }
                ExecutionResult::Revert { gas_used, output } => {
                    ExecutionResult::Revert {
                        gas_used: gas_used * token_ratio,
                        output,
                    }
                }
                ExecutionResult::Halt { reason, gas_used } => {
                    ExecutionResult::Halt {
                        reason,
                        gas_used: gas_used * token_ratio,
                    }
                }
            }
        } else {
            result
        };

        Ok((final_result.into(), inspector_collect(inspector)))
    }
}

impl<DB> ApiCore for MantleApiImpl<DB> where DB: Sync + Send + 'static {}
