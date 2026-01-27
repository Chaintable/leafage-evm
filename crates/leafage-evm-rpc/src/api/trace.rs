use jsonrpsee::proc_macros::rpc;
use leafage_evm_types::{BlockId, DebankOutPut};

#[rpc(client, namespace = "trace")]
#[async_trait::async_trait]
pub trait TraceApi {
    // get block storage diff  from (block_number-1,block_number]
    #[method(name = "debankBlock")]
    async fn debank_block(&self, block_id: BlockId) -> RpcResult<DebankOutPut>;
}
