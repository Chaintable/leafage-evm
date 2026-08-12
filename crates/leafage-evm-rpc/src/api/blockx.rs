use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use leafage_evm_types::{BlockxStateReadBatch, BlockxStateReadBatchResp};

/// BlockX-internal namespace. `blockx_stateReadBatch` resolves a batch
/// of `getAddressCode` / `getStorageAt` reads against one state view of
/// a fixed block. It is not part of the public SDK surface, but it is
/// validated and rate-limited like any other network input.
#[rpc(server, client, namespace = "blockx")]
#[async_trait::async_trait]
pub trait BlockxApi {
    #[method(name = "stateReadBatch")]
    async fn state_read_batch(
        &self,
        batch: BlockxStateReadBatch,
    ) -> RpcResult<BlockxStateReadBatchResp>;
}
