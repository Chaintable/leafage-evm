use crate::api_impl::core::{Api, GetHaltReason, GetTransactionError, ToJsonRpcError, TxSetter};
use crate::api_impl::{ApiCore, EvmExecutor};
use crate::DebankApiServer;
use alloy::rpc::types::TransactionRequest;
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockIndex, EvmStorageRead};
use leafage_evm_types::{Block, BlockType, DebankBlockContext, DebankErrorCode, DebankTransaction};
use tracing::{debug, info};

impl<C> Api<C>
where
    C: ApiCore,
    C::DB: EvmStorageRead + BlockIndex,
    C::Tx: TxSetter + Clone,
    C::TransactionError: ToJsonRpcError + GetTransactionError,
    C::EvmHaltReason: std::fmt::Debug + Clone + GetHaltReason,
    DebankErrorCode: From<<C as EvmExecutor>::EvmHaltReason>,
{
    pub(crate) async fn replay_blocks(
        &self,
        blocks: Vec<Block<DebankTransaction>>,
    ) -> RpcResult<()> {
        let start = std::time::Instant::now();
        let block_len = blocks.len();
        let transactions_len = blocks.iter().map(|b| b.transactions.len()).sum::<usize>();
        info!(target: "warmup","Start replay blocks with {block_len} blocks {transactions_len} transactions");
        for block in blocks {
            let transactions = block.transactions.into_transactions_vec();
            let transactions_len = transactions.len();
            let calls: Vec<_> = transactions
                .into_iter()
                .map(|tx| {
                    let mut transaction_request: TransactionRequest = tx.into();
                    transaction_request.gas_price = Some(0);
                    transaction_request.max_fee_per_gas = None;
                    transaction_request.max_priority_fee_per_gas = None;
                    transaction_request.max_fee_per_gas = None;
                    transaction_request
                })
                .collect();
            self.simulate_transactions(
                calls,
                Some(DebankBlockContext {
                    block_id: block.header.parent_hash.into(),
                    block_type: BlockType::Equals,
                }),
                None,
            )
            .await?;
            debug!(target: "warmup","Replay {} block {} transactions complete",block.header.number,transactions_len);
        }
        info!(target: "warmup", "Replay {block_len} blocks {transactions_len} transactions time elapsed: {:?}", start.elapsed());
        Ok(())
    }
}
