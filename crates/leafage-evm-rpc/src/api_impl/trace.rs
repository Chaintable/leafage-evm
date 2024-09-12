use crate::api::TraceApiServer;
use crate::api_impl::utils::create_txn_env;
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockIndex, EvmStorageRead, EvmStorageWrapper, TransactionIndex};
use leafage_evm_types::{
    block_env_from_block, Block, BlockId, BlockNumberOrTag, Bytes, CallRequest,
    LocalizedTransactionTrace, Transaction, TransactionInfo, H256,
};
use revm::db::CacheDB;
use revm::primitives::{CfgEnv, CfgEnvWithHandlerCfg, EnvWithHandlerCfg, SpecId};
use revm::{inspector_handle_register, Evm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::error;

/// [`TraceApiImpl`] implements the EthApi trait.
pub struct TraceApiImpl<DB> {
    db: DB,
    cfg: CfgEnv,
}

impl<DB: EvmStorageRead + TransactionIndex + BlockIndex> TraceApiImpl<DB> {
    pub fn new(db: DB, cfg: CfgEnv) -> Self {
        Self { db, cfg }
    }

    async fn trace_transaction_impl(
        &self,
        hash: H256,
    ) -> RpcResult<Option<Vec<LocalizedTransactionTrace>>> {
        let cfg = self.cfg.clone();
        let txn = self
            .db
            .get_transaction_by_hash(hash)
            .map_err(|e| internal_rpc_err(e.to_string()))?
            .ok_or_else(|| invalid_params_rpc_err("Transaction not found"))?;
        let block = self
            .db
            .get_block_by_hash_arc(txn.block_hash.unwrap())
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
            if tx.hash == hash {
                break;
            }
            txs_before.push(tx.clone().into_request());
        }
        let (tx, rx) = oneshot::channel();

        tokio::task::spawn_blocking(move || {
            let rsp = Self::call_and_trace(txs_before, txn, cfg, state, block);
            if let Err(e) = tx.send(rsp) {
                error!("Failed to call_and_trace, result: {:?}", e);
            }
        });

        let rsp = rx
            .await
            .map_err(|_| internal_rpc_err("trace failed".to_string()))?;
        rsp.map(Some)
    }

    fn call_and_trace(
        brefore_txs: Vec<CallRequest>,
        trace_tx: Transaction,
        cfg: CfgEnv,
        state: DB::StateDB,
        block: Arc<Block<Transaction>>,
    ) -> RpcResult<Vec<LocalizedTransactionTrace>> {
        let block_env = block_env_from_block(&block);
        let mut memory_db = CacheDB::new(EvmStorageWrapper(state));
        let cfg = CfgEnvWithHandlerCfg::new_with_spec_id(cfg.clone(), SpecId::LATEST);
        for tx in brefore_txs {
            let tx = create_txn_env(&block_env, tx)?;
            let env = EnvWithHandlerCfg::new_with_cfg_env(cfg.clone(), block_env.clone(), tx);
            let mut evm = Evm::builder()
                .with_db(&mut memory_db)
                .with_env_with_handler_cfg(env)
                .build();
            let _ = evm
                .transact_commit()
                .map_err(|e| internal_rpc_err(e.to_string()))?;
        }
        let tx_info = TransactionInfo::from(&trace_tx);
        let tx = create_txn_env(&block_env, trace_tx.into_request())?;
        let env = EnvWithHandlerCfg::new_with_cfg_env(cfg, block_env, tx);

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
}

#[async_trait::async_trait]
impl<DB> TraceApiServer for TraceApiImpl<DB>
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
        unimplemented!()
    }
}
