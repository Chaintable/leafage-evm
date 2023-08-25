use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use leafage_evm_types::{BlockId, Bytes, CallRequest};

#[rpc(server, client, namespace = "eth")]
#[async_trait::async_trait]
pub trait EthApi {
    // Executes a new message call immediately without creating a transaction on the block chain.
    #[method(name = "call")]
    async fn call(&self, request: CallRequest, block_number: BlockId) -> RpcResult<Bytes>;
}
