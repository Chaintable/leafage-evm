use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use leafage_evm_types::Bytes;

/// BlockX-internal namespace. `blockx_stateReadBatch` resolves a batch
/// of `getAddressCode` / `getStorageAt` reads against one state view of
/// a fixed block. The parameter and result are hex-encoded BSRB/1
/// binary payloads (see `leafage_evm_types::rpc::blockx`): the JSON-RPC
/// shell exists so nodex-proxy keeps routing by method name, while the
/// payload itself stays cheap to build and parse inside BlockX's
/// executor sandbox. Not part of the public SDK surface, but validated
/// and rate-limited like any other network input.
#[rpc(server, client, namespace = "blockx")]
#[async_trait::async_trait]
pub trait BlockxApi {
    #[method(name = "stateReadBatch")]
    async fn state_read_batch(&self, payload: Bytes) -> RpcResult<Bytes>;
}
