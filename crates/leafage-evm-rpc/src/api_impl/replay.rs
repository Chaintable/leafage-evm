use crate::api_impl::core::ToJsonRpcError;
use crate::api_impl::{ApiBase, ApiCore, EvmExecutor};
use crate::error::internal_rpc_err;
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockContext, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{block_env_from_block, Block, DebankTransaction, TransactionInfo, H256};
use revm::database::CacheDB;
use revm_inspectors::tracing::TracingInspectorConfig;
use serde::de::StdError;
use tracing::info;

pub trait Replyable: ApiCore {
    fn replay_blocks(&self, blocks: Vec<Block<DebankTransaction>>) -> RpcResult<()>;
}

impl<Api> Replyable for Api
where
    Api: ApiCore,
    <Api as ApiBase>::DB: EvmStorageRead,
    <Api as EvmExecutor>::TransactionError: Sync + Send + StdError,
{
    fn replay_blocks(&self, blocks: Vec<Block<DebankTransaction>>) -> RpcResult<()> {
        let start = std::time::Instant::now();
        let block_len = blocks.len();
        info!(target: "warmup","Start replay blocks with length {block_len}");
        for block in blocks {
            let block_id = block.header.hash.into();
            let transactions = block.transactions.into_transactions_vec();
            let state = self
                .db()
                .state_at(block_id)
                .map_err(|e| internal_rpc_err(e.to_string()))?;
            let Some(state) = state else {
                if self.evm_cfg().is_archive {
                    return Err(internal_rpc_err(format!(
                        "block block_id {:?} is invalid",
                        block_id
                    )))?;
                } else {
                    return Err(internal_rpc_err(format!(
                        "block block_id {:?} not found for state node",
                        block_id
                    )))?;
                }
            };
            let block = state
                .block_info_arc()
                .map_err(|e| internal_rpc_err(e.to_string()))?;
            let block_env = block_env_from_block(&block);
            let mut memory_db = CacheDB::new(EvmStorageWrapper {
                db: state,
                ovm_address: self.evm_cfg().ovm_address,
                normalize_state_key: self.evm_cfg().normalize_state_key,
            });
            for (index, transaction) in transactions.into_iter().enumerate() {
                let tx_info = TransactionInfo {
                    hash: Some(H256::random()),
                    index: Some(index as _),
                    block_hash: Some(block.header.hash),
                    block_number: Some(block.header.number),
                    base_fee: block.header.base_fee_per_gas,
                };
                let trace_cfg = TracingInspectorConfig::default_parity();
                let tx = self.create_txn_env(
                    &block_env,
                    transaction.into(),
                    &memory_db,
                    self.evm_cfg().cfg.chain_id,
                )?;
                let (_, _) = self
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
            }
        }
        info!(target: "warmup", "Replay blocks {} time elapsed: {:?}",block_len, start.elapsed());
        Ok(())
    }
}
