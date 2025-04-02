use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use leafage_evm_types::{BlockId, Bytes};

#[rpc(server, client, namespace = "trace")]
#[async_trait::async_trait]
pub trait TraceApi {
    // get block storage diff  from (block_number-1,block_number]
    #[method(name = "blockStateDiff")]
    async fn block_state_diff(&self, block_id: BlockId, re_exec: bool) -> RpcResult<Bytes>;
}
