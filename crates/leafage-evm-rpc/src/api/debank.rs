use alloy::rpc::types::state::StateOverride;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use leafage_evm_types::{
    Address, BlockOverrides, Bytes, CallRequest, DebankBlock, DebankBlockContext,
    DebankMultiCallResp, DebankSimulateResp, JsonStorageKey, H256, U256,
};

#[rpc(server, client)]
#[async_trait::async_trait]
pub trait DebankApi {
    #[method(name = "version")]
    async fn version(&self) -> RpcResult<String>;

    #[method(name = "getAddressNonce")]
    async fn get_address_nonce(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<U256>;

    #[method(name = "getAddressBalance")]
    async fn get_address_balance(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<U256>;

    #[method(name = "getAddressCode")]
    async fn get_address_code(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<Bytes>;

    #[method(name = "getStorageAt")]
    async fn get_storage_at(
        &self,
        address: Address,
        position: JsonStorageKey,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<H256>;

    #[method(name = "contractMultiCall")]
    async fn contract_multi_call(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
        state_override: Option<StateOverride>,
        fast_fail: Option<bool>,
        use_parallel: Option<bool>,
        disable_cache: Option<bool>,
    ) -> RpcResult<DebankMultiCallResp>;

    #[method(name = "simulateTransactions")]
    async fn simulate_transactions(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<DebankSimulateResp>;

    #[method(name = "getLatestBlock")]
    async fn get_latest_block(&self) -> RpcResult<DebankBlock>;

    #[method(name = "getBlockByHeight")]
    async fn get_block_by_height(&self, height: U256) -> RpcResult<DebankBlock>;

    #[method(name = "getBlockById")]
    async fn get_block_by_id(&self, id: H256) -> RpcResult<DebankBlock>;

    #[method(name = "blockIsValid")]
    async fn block_is_valid(&self, id: H256) -> RpcResult<bool>;

    #[method(name = "estimateGas")]
    async fn estimate_gas(
        &self,
        request: CallRequest,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<U256>;
}
