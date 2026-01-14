use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use leafage_evm_types::{BlockId, CallRequest, DefaultFrame, PreResult};

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

    /// Trace a single call and return struct logs.
    #[method(name = "traceCall")]
    async fn trace_call(
        &self,
        request: CallRequest,
        block_id: Option<BlockId>,
    ) -> RpcResult<DefaultFrame>;
}
