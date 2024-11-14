use super::ApiImpl;
use crate::api::TraceApiServer;
use crate::api_impl::utils::{get_handler_cfg, rebuild_txn_env};
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use alloy::network::TransactionResponse;
use alloy_rlp::{BytesMut, Encodable};
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{
    BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrapper, TransactionIndex,
};
use leafage_evm_types::{
    block_env_from_block, Block, BlockId, Bytes, LocalizedTransactionTrace, Transaction,
    TransactionInfo, H256,
};
use revm::db::CacheDB;
use revm::primitives::{CfgEnv, EnvWithHandlerCfg, SpecId};
use revm::{inspector_handle_register, Evm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::error;

impl<DB: EvmStorageRead + TransactionIndex + BlockIndex> ApiImpl<DB> {
    async fn trace_transaction_impl(
        &self,
        hash: H256,
    ) -> RpcResult<Option<Vec<LocalizedTransactionTrace>>> {
        let cfg = self.cfg.clone();

        let spec_id = self.spec_id;

        let tx = self
            .db
            .get_transaction_by_hash(hash)
            .map_err(|e| internal_rpc_err(e.to_string()))?
            .ok_or_else(|| invalid_params_rpc_err("Transaction not found"))?;
        #[cfg(not(feature = "optimism"))]
        let block = self
            .db
            .get_block_by_id_arc(tx.block_hash.unwrap().into())
            .map_err(|e| {
                internal_rpc_err(format!("Failed to get block by hash: {}", e.to_string()))
            })?;

        #[cfg(feature = "optimism")]
        let block = self
            .db
            .get_block_by_id_arc(tx.inner.block_hash.unwrap().into())
            .map_err(|e| {
                internal_rpc_err(format!("Failed to get block by hash: {}", e.to_string()))
            })?;
        if block.is_none() {
            return Ok(None);
        }
        let block = block.unwrap();
        let block_id = BlockId::Hash(block.header.parent_hash.into());
        let state = self
            .db
            .state_at(block_id)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = state.unwrap();
        let mut txs_before = Vec::new();
        for tx in block.transactions.txns() {
            if tx.tx_hash() == hash {
                break;
            }
            txs_before.push(tx.clone());
        }

        let (sender, receiver) = oneshot::channel();

        tokio::task::spawn_blocking(move || {
            let rsp = Self::call_and_trace(txs_before, tx, cfg, spec_id, state, block);
            if let Err(e) = sender.send(rsp) {
                error!("Failed to call_and_trace, result: {:?}", e);
            }
        });

        let rsp = receiver
            .await
            .map_err(|_| internal_rpc_err("trace failed".to_string()))?;
        rsp.map(Some)
    }

    fn call_and_trace(
        brefore_txs: Vec<Transaction>,
        trace_tx: Transaction,
        cfg: CfgEnv,
        spec_id: SpecId,
        state: DB::StateDB,
        block: Arc<Block<Transaction>>,
    ) -> RpcResult<Vec<LocalizedTransactionTrace>> {
        let block_env = block_env_from_block(&block);
        let mut memory_db = CacheDB::new(EvmStorageWrapper(state));
        let cfg = get_handler_cfg(cfg.clone(), spec_id);
        for tx in brefore_txs {
            let tx_env = rebuild_txn_env(&tx);
            let env = EnvWithHandlerCfg::new_with_cfg_env(cfg.clone(), block_env.clone(), tx_env);
            let mut evm = Evm::builder()
                .with_db(&mut memory_db)
                .with_env_with_handler_cfg(env)
                .build();
            let _ = evm
                .transact_commit()
                .map_err(|e| internal_rpc_err(e.to_string()))?;
        }

        #[cfg(not(feature = "optimism"))]
        let tx_info = TransactionInfo::from(&trace_tx);
        #[cfg(feature = "optimism")]
        let tx_info = TransactionInfo {
            hash: Some(trace_tx.tx_hash()),
            block_hash: trace_tx.block_hash(),
            block_number: trace_tx.block_number(),
            index: trace_tx.transaction_index(),
            base_fee: None,
        };

        let tx_env = rebuild_txn_env(&trace_tx);

        let env = EnvWithHandlerCfg::new_with_cfg_env(cfg, block_env, tx_env);

        let trace_cfg = TracingInspectorConfig::default_parity();
        let mut inspector = TracingInspector::new(trace_cfg);

        let mut evm = Evm::builder()
            .with_db(&mut memory_db)
            .with_external_context(&mut inspector)
            .with_env_with_handler_cfg(env)
            .append_handler_register(inspector_handle_register)
            .build();

        let _ = evm
            .transact()
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        drop(evm);

        let res = inspector
            .into_parity_builder()
            .into_localized_transaction_traces(tx_info);
        Ok(res)
    }

    async fn block_state_diff_impl(&self, block_id: BlockId, _re_exec: bool) -> RpcResult<Bytes> {
        let state = self
            .db
            .state_at(block_id)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = state.unwrap();
        let diff = state
            .state_diff_arc()
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let mut buffer = BytesMut::new();
        diff.as_ref().encode(&mut buffer);
        Ok(buffer.freeze().into())
    }
}

#[async_trait::async_trait]
impl<DB> TraceApiServer for ApiImpl<DB>
where
    DB: EvmStorageRead + TransactionIndex + BlockIndex + Send + Sync + 'static,
{
    async fn trace_transaction(
        &self,
        hash: H256,
    ) -> RpcResult<Option<Vec<LocalizedTransactionTrace>>> {
        self.trace_transaction_impl(hash).await
    }

    async fn block_state_diff(&self, _block_id: BlockId, _re_exec: bool) -> RpcResult<Bytes> {
        self.block_state_diff_impl(_block_id, _re_exec).await
    }
}
