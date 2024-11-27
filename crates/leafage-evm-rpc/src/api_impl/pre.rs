use super::ApiImpl;
use crate::api::PreApiServer;
use crate::api_impl::utils::{create_txn_env, get_handler_cfg};
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use alloy::sol_types::decode_revert_reason;
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockContext, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{
    block_env_from_block, Block, BlockId, BlockNumberOrTag, CallRequest, ExecutionResult,
    HaltReason, Log, PreError, PreErrorCode, PreResult, Transaction, TransactionInfo, H256,
};
use revm::db::CacheDB;
use revm::primitives::{CfgEnv, EnvWithHandlerCfg, SpecId};
use revm::{inspector_handle_register, Evm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::error;

impl<DB: EvmStorageRead> ApiImpl<DB> {
    async fn pre_trace_many_impl(
        &self,
        requests: Vec<CallRequest>,
        block_id: Option<BlockId>,
    ) -> RpcResult<Vec<PreResult>> {
        let mut cfg = self.cfg.clone();
        cfg.disable_eip3607 = true;
        cfg.disable_base_fee = true;
        cfg.disable_block_gas_limit = true;

        let spec_id = self.spec_id;

        let state = self
            .db
            .state_at(block_id.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest)))
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = state.unwrap();
        let block = state
            .block_info_arc()
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let (tx, rx) = oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let rsp = Self::call_many_and_trace(requests, cfg, spec_id, state, block);
            if let Err(e) = tx.send(rsp) {
                error!("Failed to send multi_call result: {:?}", e);
            }
        });
        let rsp = rx
            .await
            .map_err(|_| internal_rpc_err("PreTraceMany failed".to_string()))?;
        rsp
    }

    fn call_many_and_trace(
        txs: Vec<CallRequest>,
        cfg: CfgEnv,
        spec_id: SpecId,
        state: DB::StateDB,
        block: Arc<Block<Transaction>>,
    ) -> RpcResult<Vec<PreResult>> {
        let block_env = block_env_from_block(&block);
        let mut memory_db = CacheDB::new(EvmStorageWrapper(state));
        let cfg = get_handler_cfg(cfg, spec_id);
        let mut tx_index: u64 = 0;
        let mut log_index = 0;
        let mut pre_results: Vec<PreResult> = Vec::new();
        for tx in txs {
            let tx_info = TransactionInfo {
                hash: Some(H256::random()),
                index: Some(tx_index),
                block_hash: Some(block.header.hash),
                block_number: Some(block.header.number),
                base_fee: block.header.base_fee_per_gas.map(|x| x as u128),
            };
            tx_index += 1;
            if let Some(last_res) = pre_results.last() {
                if last_res.error.code != 0 {
                    pre_results.push(last_res.clone());
                    continue;
                }
            }
            let tx = create_txn_env(&block_env, tx)?;
            let trace_cfg = TracingInspectorConfig::default_parity();
            let mut inspector = TracingInspector::new(trace_cfg);
            let env = EnvWithHandlerCfg::new_with_cfg_env(cfg.clone(), block_env.clone(), tx);
            let mut evm = Evm::builder()
                .with_db(&mut memory_db)
                .with_external_context(&mut inspector)
                .with_env_with_handler_cfg(env)
                .append_handler_register(inspector_handle_register)
                .build();
            let exec_res = evm
                .transact_commit()
                .map_err(|e| internal_rpc_err(e.to_string()))?;
            drop(evm);
            match exec_res {
                ExecutionResult::Revert { gas_used, output } => {
                    let reason =
                        decode_revert_reason(&output).unwrap_or("Reason Unknown".to_string());
                    let pre_error = PreError {
                        msg: reason,
                        code: PreErrorCode::Reverted as i64,
                    };
                    let pre_res = PreResult {
                        error: pre_error,
                        gas_used,
                        ..Default::default()
                    };
                    pre_results.push(pre_res);
                }
                ExecutionResult::Halt { reason, gas_used } => {
                    let code = match reason {
                        HaltReason::OutOfFunds => PreErrorCode::InsufficientBalane as i64,
                        _ => PreErrorCode::UnKnown as i64,
                    };
                    let pre_error = PreError {
                        msg: format!("{:?}", reason),
                        code,
                    };
                    let pre_res = PreResult {
                        error: pre_error,
                        gas_used,
                        ..Default::default()
                    };
                    pre_results.push(pre_res);
                }
                ExecutionResult::Success { gas_used, logs, .. } => {
                    let trace_res = inspector
                        .into_parity_builder()
                        .into_localized_transaction_traces(tx_info);
                    let mut trace_logs = vec![];
                    for log in logs {
                        trace_logs.push(Log {
                            inner: log,
                            block_hash: Some(block.header.hash),
                            block_number: Some(block.header.number),
                            block_timestamp: Some(block.header.timestamp),
                            transaction_hash: Some(tx_info.hash.unwrap()),
                            transaction_index: Some(tx_info.index.unwrap()),
                            log_index: Some(log_index),
                            removed: false,
                        });
                        log_index += 1;
                    }
                    let pre_res = PreResult {
                        gas_used,
                        logs: trace_logs,
                        trace: trace_res,
                        ..Default::default()
                    };
                    pre_results.push(pre_res);
                }
            }
        }
        Ok(pre_results)
    }
}

#[async_trait::async_trait]
impl<DB> PreApiServer for ApiImpl<DB>
where
    DB: EvmStorageRead + Send + Sync + 'static,
{
    async fn trace_many(
        &self,
        tx_hash: Vec<CallRequest>,
        block_id: Option<BlockId>,
    ) -> RpcResult<Vec<PreResult>> {
        self.pre_trace_many_impl(tx_hash, block_id).await
    }
}
