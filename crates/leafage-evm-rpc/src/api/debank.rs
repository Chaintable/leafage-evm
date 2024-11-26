use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use leafage_evm_types::{
    Address, BlockContext, BlockId, BlockOverrides, BlockType, Bytes, CallRequest, MultiCallResp,
    U256,
};

#[rpc(server, client)]
#[async_trait::async_trait]
pub trait DebankApi {
    #[method(name = "getAddressNonce")]
    async fn get_address_nonce(
        &self,
        address: Address,
        block_ctx: Option<BlockContext>,
    ) -> RpcResult<U256>;

    #[method(name = "getAddressBalance")]
    async fn get_address_balance(
        &self,
        address: Address,
        block_ctx: Option<BlockContext>,
    ) -> RpcResult<U256>;

    #[method(name = "getAddressCode")]
    async fn get_address_code(
        &self,
        address: Address,
        block_ctx: Option<BlockContext>,
    ) -> RpcResult<Bytes>;

    #[method(name = "contractMultiCall")]
    async fn contract_multi_call(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<BlockContext>,
        block_overrides: Option<BlockOverrides>,
        fast_fail: Option<bool>,
        use_parallel: Option<bool>,
        disable_cache: Option<bool>,
    ) -> RpcResult<MultiCallResp>;

    #[method(name = "simulateTransactions")]
    async fn simulate_transactions(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<BlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<()>;
}
