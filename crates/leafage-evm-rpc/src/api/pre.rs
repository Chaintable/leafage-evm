use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use leafage_evm_types::{BlockId, CallRequest, PreResult};

#[rpc(server, client, namespace = "pre")]
#[async_trait::async_trait]
pub trait PreApi {
    /// Trace many transactions.
    #[method(name = "traceMany")]
    async fn trace_many(
        &self,
        tx_hash: Vec<CallRequest>,
        block_id: Option<BlockId>,
    ) -> RpcResult<Vec<PreResult>>;
}
