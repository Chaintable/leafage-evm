use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use leafage_evm_types::{BlockId, Bytes};

#[rpc(server, client, namespace = "leafage")]
#[async_trait::async_trait]
pub trait LeafAgeApi {
    // get block storage diff  from (block_number-1,block_number]
    #[method(name = "blockDiff")]
    async fn block_diff(&self, block_id: BlockId, re_exec: bool) -> RpcResult<Bytes>;
}
