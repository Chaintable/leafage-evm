use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use leafage_evm_types::{
    Address, BlockId, BlockNumber, Bytes, CallRequest, JsonStorageKey, MultiCallResp, H256, U256,
};
use serde_json::Value;

#[rpc(server, client, namespace = "eth")]
#[async_trait::async_trait]
pub trait EthApi {
    // Executes a new message call immediately without creating a transaction on the block chain.
    #[method(name = "call")]
    async fn call(&self, request: CallRequest, block_number: BlockId) -> RpcResult<Bytes>;

    #[method(name = "multiCall")]
    async fn multi_call(
        &self,
        requests: Vec<CallRequest>,
        block_number: BlockId,
        fast_fail: bool,
        use_parallel: bool,
        disable_cache: bool,
    ) -> RpcResult<MultiCallResp>;

    #[method(name = "blockNumber")]
    async fn block_number(&self) -> RpcResult<U256>;

    #[method(name = "getBalance")]
    async fn get_balance(&self, address: Address, block_number: BlockId) -> RpcResult<U256>;

    #[method(name = "getBlockByNumber")]
    async fn get_block_by_number(
        &self,
        block_number: BlockNumber,
        full: bool,
    ) -> RpcResult<Option<Value>>;

    #[method(name = "getBlockByHash")]
    async fn get_block_by_hash(&self, block_hash: H256, full: bool) -> RpcResult<Option<Value>>;

    #[method(name = "getCode")]
    async fn get_code(&self, address: Address, block_number: BlockId) -> RpcResult<Bytes>;

    #[method(name = "getStorageAt")]
    async fn get_storage_at(
        &self,
        address: Address,
        position: JsonStorageKey,
        block_number: BlockId,
    ) -> RpcResult<H256>;

    #[method(name = "getTransactionCount")]
    async fn get_transaction_count(
        &self,
        address: Address,
        block_number: BlockId,
    ) -> RpcResult<U256>;

    #[method(name = "chainId")]
    async fn chain_id(&self) -> RpcResult<U256>;
}
