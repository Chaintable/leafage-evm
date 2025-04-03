use super::ApiImpl;
use crate::api::TraceApiServer;
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use alloy_rlp::{BytesMut, Encodable};
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockContext, BlockIndex, EvmStorageRead};
use leafage_evm_types::{BlockId, Bytes};

impl<DB: EvmStorageRead + BlockIndex> ApiImpl<DB> {
    async fn block_state_diff_impl(&self, block_id: BlockId, _re_exec: bool) -> RpcResult<Bytes> {
        let state = self
            .db
            .state_at(block_id)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = state.unwrap();
        let diff = state
            .state_diff_arc()
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let mut buffer = BytesMut::new();
        diff.as_ref().encode(&mut buffer);
        Ok(buffer.freeze().into())
    }
}

#[async_trait::async_trait]
impl<DB> TraceApiServer for ApiImpl<DB>
where
    DB: EvmStorageRead + BlockIndex + Send + Sync + 'static,
{
    async fn block_state_diff(&self, _block_id: BlockId, _re_exec: bool) -> RpcResult<Bytes> {
        self.block_state_diff_impl(_block_id, _re_exec).await
    }
}
