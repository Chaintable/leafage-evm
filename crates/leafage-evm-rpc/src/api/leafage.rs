use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use leafage_evm_types::{BlockId, BlockInfo, Bytes};

#[rpc(server, client, namespace = "leafage")]
#[async_trait::async_trait]
pub trait LeafAgeApi {
    // get block storage diff  from (block_number-1,block_number]
    #[method(name = "block_diff")]
    async fn block_diff(&self, block_number: BlockId) -> RpcResult<Bytes>;

    // get block storage count from [block_number,block_number+count)
    #[method(name = "block_info")]
    async fn block_info(&self, block_number: BlockId, count: u64) -> RpcResult<Vec<BlockInfo>>;
}
