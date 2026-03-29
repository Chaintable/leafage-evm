use crate::api::PreApiServer;
use crate::api_impl::core::{
    Api, ApiCore, EvmExecutor, GetHaltReason, GetTransactionError, ToJsonRpcError, TxSetter,
};
use crate::api_impl::utils;
use crate::error::{internal_rpc_err, rpc_error_with_code};
use alloy::primitives::Bytes;
use alloy::rpc::types::trace::geth::GethDefaultTracingOptions;
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockContext, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{
    block_env_from_block, BlockId, BlockNumberOrTag, CallRequest, DebankErrorCode, DefaultFrame,
    PreErrorCode, PreResult, TransactionInfo, H256,
};
use revm::context::result::ExecutionResult;
use revm::context::Transaction as TransactionTrait;
use revm::database::CacheDB;
use revm_inspectors::tracing::TracingInspectorConfig;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::error;

impl<C> Api<C>
where
    C: ApiCore,
    C::DB: EvmStorageRead,
    C::Tx: TransactionTrait + TxSetter + Clone,
    C::TransactionError: ToJsonRpcError + GetTransactionError,
    C::EvmHaltReason: std::fmt::Debug + Clone + GetHaltReason,
    PreErrorCode: From<<C as EvmExecutor>::EvmHaltReason>,
{
    async fn pre_trace_call_impl(
        &self,
        request: CallRequest,
        block_id: Option<BlockId>,
    ) -> RpcResult<DefaultFrame> {
        let (tx, rx) = oneshot::channel();
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let rsp = this.pre_trace_call_impl_inner(request, block_id);
            if let Err(e) = tx.send(rsp) {
                error!("Failed to send trace_call result: {:?}", e);
            }
        });
        let rsp = rx
            .await
            .map_err(|_| internal_rpc_err("PreTraceCall failed".to_string()))?;
        rsp
    }

    fn pre_trace_call_impl_inner(
        &self,
        request: CallRequest,
        block_id: Option<BlockId>,
    ) -> RpcResult<DefaultFrame> {
        let state = self
            .inner
            .db()
            .state_at(block_id.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest)))
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            if self.inner.evm_cfg().is_archive {
                return Err(rpc_error_with_code(
                    DebankErrorCode::InvalidBlockID as i32,
                    format!("block block_id {:?} is invalid", block_id),
                ));
            } else {
                return Err(rpc_error_with_code(
                    DebankErrorCode::BlockNotFound as i32,
                    format!("block block_id {:?} not found for state node", block_id),
                ));
            }
        }
        let state = state.unwrap();
        let block = state
            .block_info_arc()
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let block_env = block_env_from_block(&block);
        let mut memory_db = CacheDB::new(EvmStorageWrapper {
            db: state,
            ovm_address: self.inner.evm_cfg().ovm_address,
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        });

        // Use geth config to enable step recording for struct logs
        let trace_cfg = TracingInspectorConfig::default_geth();
        let tx = self.inner.create_txn_env(
            &block_env,
            request,
            &memory_db,
            self.inner.evm_cfg().cfg.chain_id,
        )?;

        let (exec_res, inspector) = self
            .inner
            .inspect_tx_commit(
                &block_env,
                &mut memory_db,
                trace_cfg,
                |inspector| inspector,
                tx,
            )
            .map_err(|e| e.to_rpc_error())?;

        // Extract gas_used and return_value from execution result
        let (gas_used, return_value) = match &exec_res {
            ExecutionResult::Success {
                gas, output, ..
            } => (gas.used(), output.data().clone()),
            ExecutionResult::Revert { gas, output, .. } => (gas.used(), output.clone()),
            ExecutionResult::Halt { gas, .. } => (gas.used(), Bytes::new()),
        };

        // Build geth traces with default options
        let geth_opts = GethDefaultTracingOptions::default();
        let frame = inspector
            .into_geth_builder()
            .geth_traces(gas_used, return_value, geth_opts);

        Ok(frame)
    }

    async fn pre_trace_many_impl(
        &self,
        requests: Vec<CallRequest>,
        block_id: Option<BlockId>,
    ) -> RpcResult<Vec<PreResult>> {
        let this = self.clone();
        utils::spawn_blocking_with_cancel(move |token| {
            this.pre_trace_many_impl_inner(requests, block_id, token)
        })
        .await
        .inspect_err(|err| error!("Failed to spawn pre_trace_many result: {:?}", err))
        .map_err(|_| internal_rpc_err("pre trace many failed"))?
    }

    fn pre_trace_many_impl_inner(
        &self,
        txs: Vec<CallRequest>,
        block_id: Option<BlockId>,
        cancellation_token: CancellationToken,
    ) -> RpcResult<Vec<PreResult>> {
        let state = self
            .inner
            .db()
            .state_at(block_id.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest)))
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            if self.inner.evm_cfg().is_archive {
                return Err(rpc_error_with_code(
                    DebankErrorCode::InvalidBlockID as i32,
                    format!("block block_id {:?} is invalid", block_id),
                ));
            } else {
                return Err(rpc_error_with_code(
                    DebankErrorCode::BlockNotFound as i32,
                    format!("block block_id {:?} not found for state node", block_id),
                ));
            }
        }
        let state = state.unwrap();
        let block = state
            .block_info_arc()
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let block_env = block_env_from_block(&block);
        let mut memory_db = CacheDB::new(EvmStorageWrapper {
            db: state,
            ovm_address: self.inner.evm_cfg().ovm_address,
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        });
        let mut tx_index: u64 = 0;
        let mut log_index = 0;
        let mut pre_results: Vec<PreResult> = Vec::new();
        for tx in txs {
            if cancellation_token.is_cancelled() {
                return Err(internal_rpc_err(
                    "pre trace many cancelled by caller".to_string(),
                ));
            }
            let tx_info = TransactionInfo {
                hash: Some(H256::random()),
                index: Some(tx_index),
                block_hash: Some(block.header.hash),
                block_number: Some(block.header.number),
                base_fee: block.header.base_fee_per_gas,
            };
            tx_index += 1;
            if let Some(last_res) = pre_results.last() {
                if last_res.error.code != 0 {
                    pre_results.push(last_res.clone());
                    continue;
                }
            }
            let trace_cfg = TracingInspectorConfig::default_parity();
            let tx = self.inner.create_txn_env(
                &block_env,
                tx,
                &memory_db,
                self.inner.evm_cfg().cfg.chain_id,
            )?;
            let (exec_res, traces) = self
                .inner
                .inspect_tx_commit(
                    &block_env,
                    &mut memory_db,
                    trace_cfg,
                    |inspector| {
                        inspector
                            .into_parity_builder()
                            .into_localized_transaction_traces(tx_info)
                    },
                    tx,
                )
                .map_err(|e| e.to_rpc_error())?;
            let mut pre_res: PreResult = exec_res.into();
            for log in &mut pre_res.logs {
                log.log_index = Some(log_index);
                log.block_hash = Some(block.header.hash);
                log.block_number = Some(block.header.number);
                log.block_timestamp = Some(block.header.timestamp);
                log.transaction_hash = Some(tx_info.hash.unwrap());
                log.transaction_index = Some(tx_info.index.unwrap());
                log_index += 1;
            }
            pre_res.trace = traces;
            pre_results.push(pre_res);
        }
        Ok(pre_results)
    }
}

#[async_trait::async_trait]
impl<C> PreApiServer for Api<C>
where
    C: ApiCore,
    C::DB: EvmStorageRead,
    C::Tx: TransactionTrait + TxSetter + Clone,
    C::TransactionError: ToJsonRpcError + GetTransactionError,
    C::EvmHaltReason: std::fmt::Debug + Clone + GetHaltReason,
    PreErrorCode: From<<C as EvmExecutor>::EvmHaltReason>,
{
    async fn trace_many(
        &self,
        tx_hash: Vec<CallRequest>,
        block_id: Option<BlockId>,
    ) -> RpcResult<Vec<PreResult>> {
        self.pre_trace_many_impl(tx_hash, block_id).await
    }

    async fn trace_call(
        &self,
        request: CallRequest,
        block_id: Option<BlockId>,
    ) -> RpcResult<DefaultFrame> {
        self.pre_trace_call_impl(request, block_id).await
    }
}
