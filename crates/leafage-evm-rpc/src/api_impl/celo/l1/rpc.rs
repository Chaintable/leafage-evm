//! Celo L1 wire semantics. Execution and gas estimation live in the chain EVM.
use super::{input_error, ArchiveChain, CeloL1ApiImpl};
use crate::{
    api::{DebankApiClient, EthApiServer},
    api_impl::{
        core::{Api, ApiBase, EvmExecutor, ToJsonRpcError},
        debank::combine_error_message,
        historical_overload::{historical_rpc_overloaded_error, is_historical_rpc_overloaded},
        utils,
    },
    error::{database_rpc_err, internal_rpc_err, rpc_err, rpc_error_with_code},
};
use alloy::{rpc::types::state::StateOverride, sol_types::decode_revert_reason};
use jsonrpsee::{core::RpcResult, proc_macros::rpc, RpcModule};
use leafage_evm_chains::celo::l1::{
    estimate::{estimate, EstimateError},
    GINGERBREAD_BLOCK,
};
use leafage_evm_storage::{BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{
    block_env_from_block, BlockId, BlockNumberOrTag, BlockOverrides, Bytes, CallRequest,
    DebankBlockContext, DebankErrorCode, H256, U256,
};
use revm::{context::result::ExecutionResult, database::CacheDB};
use serde_json::Value;

#[rpc(server)]
trait CeloL1Rpc {
    #[method(name = "eth_call")]
    async fn call(
        &self,
        request: CallRequest,
        block_number: BlockId,
        state_override: Option<StateOverride>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<Bytes>;
    #[method(name = "estimateGas")]
    async fn estimate_gas(
        &self,
        request: CallRequest,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<U256>;
    #[method(name = "eth_getBlockByNumber")]
    async fn get_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<Value>>;
    #[method(name = "eth_getBlockByHash")]
    async fn get_block_by_hash(&self, block_hash: H256, full: bool) -> RpcResult<Option<Value>>;
}

struct CeloRpc<DB>(Api<CeloL1ApiImpl<DB>>);

pub(crate) fn register<DB>(
    module: &mut RpcModule<()>,
    api: Api<CeloL1ApiImpl<DB>>,
) -> std::io::Result<()>
where
    DB: EvmStorageRead + BlockIndex + Send + Sync + 'static,
{
    for name in [
        "eth_call",
        "estimateGas",
        "eth_getBlockByNumber",
        "eth_getBlockByHash",
    ] {
        module
            .remove_method(name)
            .ok_or_else(|| std::io::Error::other(format!("missing Celo RPC {name}")))?;
    }
    module
        .merge(CeloL1RpcServer::into_rpc(CeloRpc(api)))
        .map_err(std::io::Error::other)
}

fn format_block(mut block: Option<Value>) -> Option<Value> {
    if let Some(value) = block.as_mut() {
        let number = value
            .get("number")
            .and_then(Value::as_str)
            .and_then(|number| u64::from_str_radix(number.trim_start_matches("0x"), 16).ok());
        if number.is_some_and(|number| number < GINGERBREAD_BLOCK) {
            if let Some(object) = value.as_object_mut() {
                for key in ["nonce", "mixHash", "sha3Uncles", "uncles"] {
                    object.remove(key);
                }
            }
        }
    }
    block
}

#[async_trait::async_trait]
impl<DB> CeloL1RpcServer for CeloRpc<DB>
where
    DB: EvmStorageRead + BlockIndex + Send + Sync + 'static,
{
    async fn call(
        &self,
        request: CallRequest,
        block_id: BlockId,
        state_override: Option<StateOverride>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<Bytes> {
        let api = self.0.clone();
        let limiter = api.inner.evm_cfg.exec_limiter.clone();
        utils::spawn_blocking_limited_with_cancel(limiter, move |_| {
            if let Some(collector) = api.inner.token_collector() {
                let to = request.to.and_then(|kind| kind.to().copied());
                let data = request
                    .input
                    .input()
                    .map(|data| data.as_ref())
                    .unwrap_or(&[]);
                collector.maybe_collect_call(to, data);
            }
            let state = api.resolve_state(block_id)?;
            let block = state
                .block_info_arc()
                .map_err(|error| database_rpc_err(error.to_string()))?;
            let mut block_env = block_env_from_block(&block);
            let mut db = CacheDB::new(api.inner.evm_cfg.wrap_storage(state));
            if let Some(overrides) = block_overrides {
                utils::apply_block_overrides(
                    overrides,
                    &mut db,
                    &mut block_env,
                    block.header.clone(),
                );
            }
            if let Some(overrides) = state_override {
                utils::apply_state_overrides(overrides, &mut db)?;
            }
            let tx = api.inner.create_txn_env(
                &block,
                &block_env,
                request,
                &db,
                api.inner.evm_cfg.cfg.chain_id,
            )?;
            match api
                .inner
                .transact(&block_env, &db, tx)
                .map_err(|error| error.to_rpc_error())?
            {
                ExecutionResult::Success { output, .. } => Ok(output.into_data().0.into()),
                ExecutionResult::Revert { output, .. } => Err(rpc_err(
                    DebankErrorCode::EvmRevert as i32,
                    format!(
                        "Reverted: {:?}",
                        decode_revert_reason(&output).unwrap_or("execution revert".into())
                    ),
                    Some(&output),
                )),
                ExecutionResult::Halt { reason, gas, .. } => Err(rpc_error_with_code(
                    DebankErrorCode::from(reason.clone()) as i32,
                    format!("Halted: {:?} {}", reason, gas.tx_gas_used()),
                )),
            }
        })
        .await
        .map_err(|error| internal_rpc_err(error.to_string()))?
    }

    async fn estimate_gas(
        &self,
        request: CallRequest,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<U256> {
        let historical = self.0.should_try_historical(&block_ctx).cloned();
        let original = (request.clone(), block_ctx.clone(), block_overrides.clone());
        let api = self.0.clone();
        let limiter = api.inner.evm_cfg.exec_limiter.clone();
        let result = utils::spawn_blocking_limited_with_cancel(limiter, move |cancel| {
            let state = api.debank_get_state_by_ctx_impl(block_ctx)?;
            let block = state
                .block_info_arc()
                .map_err(|error| database_rpc_err(error.to_string()))?;
            let mut block_env = block_env_from_block(&block);
            let mut db = CacheDB::new(api.inner.evm_cfg.wrap_storage(state));
            if let Some(overrides) = block_overrides {
                utils::apply_block_overrides(
                    overrides,
                    &mut db,
                    &mut block_env,
                    block.header.clone(),
                );
            }
            let db = utils::RequestCacheDB::new(db);
            estimate(
                &ArchiveChain(&api.inner.db),
                &block_env,
                api.inner.evm_cfg.cfg.clone(),
                &db,
                request,
                api.inner.evm_cfg.estimate_gas_buffer,
                || cancel.is_cancelled(),
            )
            .map(U256::from)
            .map_err(|error| match error {
                EstimateError::Input(error) => input_error(error),
                EstimateError::Execution(error) => error.to_rpc_error(),
                EstimateError::Revert(output) => rpc_err(
                    DebankErrorCode::EvmRevert as i32,
                    decode_revert_reason(&output).unwrap_or("execution revert".into()),
                    Some(&output),
                ),
                EstimateError::Halt(reason) => rpc_error_with_code(
                    DebankErrorCode::from(reason.clone()) as i32,
                    format!("Halted: {:?}", reason),
                ),
                EstimateError::Cancelled => internal_rpc_err("estimate gas cancelled by caller"),
            })
        })
        .await
        .map_err(|_| internal_rpc_err("estimate failed"))?;
        match (result, historical) {
            (Err(local), Some(client)) => client
                .estimate_gas(original.0, original.1, original.2)
                .await
                .map_err(|error| {
                    if is_historical_rpc_overloaded(&error) {
                        historical_rpc_overloaded_error()
                    } else {
                        rpc_error_with_code(
                            local.code(),
                            combine_error_message(local.message(), &error),
                        )
                    }
                }),
            (result, _) => result,
        }
    }

    async fn get_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<Value>> {
        EthApiServer::get_block_by_number(&self.0, block_number, full)
            .await
            .map(format_block)
    }
    async fn get_block_by_hash(&self, block_hash: H256, full: bool) -> RpcResult<Option<Value>> {
        EthApiServer::get_block_by_hash(&self.0, block_hash, full)
            .await
            .map(format_block)
    }
}
