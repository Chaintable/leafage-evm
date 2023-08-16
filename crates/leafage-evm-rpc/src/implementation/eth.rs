use crate::api::EthApiServer;
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use crate::implementation::utils::{create_txn_env, decode_revert_reason};
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockContext, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{BlockId, CallRequest, RpcBytes};
use revm::primitives::{CfgEnv, Env, ExecutionResult};
use revm::EVM;

pub struct EthApiImpl<DB> {
    db: DB,
    cfg: CfgEnv,
}

impl<DB: EvmStorageRead> EthApiImpl<DB> {
    pub fn new(db: DB, cfg: CfgEnv) -> Self {
        Self { db, cfg }
    }

    pub async fn call_impl(&self, request: CallRequest, block_id: BlockId) -> RpcResult<RpcBytes> {
        let mut cfg = self.cfg.clone();
        cfg.disable_eip3607 = true;
        cfg.disable_base_fee = true;
        cfg.disable_block_gas_limit = true;
        let state = self
            .db
            .state_at(block_id)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = state.unwrap();
        let block = state
            .block_info()
            .map_err(|e| internal_rpc_err(e.to_string()))?
            .into();
        let tx = create_txn_env(&block, request)?;
        let env = Env { block, cfg, tx };
        // let state =
        let mut evm = EVM::with_env(env);
        evm.database(EvmStorageWrapper(state));
        let res = evm
            .transact_ref()
            .map_err(|e| internal_rpc_err(format!("{:?}", e)))?;
        match res.result {
            ExecutionResult::Success { output, .. } => Ok(output.into_data().into()),
            ExecutionResult::Revert { output, .. } => Err(internal_rpc_err(format!(
                "Reverted: {:?}",
                decode_revert_reason(output).unwrap_or("Reason Unknown".to_string())
            ))
            .into()),
            ExecutionResult::Halt { reason, gas_used } => {
                Err(internal_rpc_err(format!("Halted: {:?} {}", reason, gas_used)).into())
            }
        }
    }
}

#[async_trait::async_trait]
impl<DB> EthApiServer for EthApiImpl<DB>
where
    DB: EvmStorageRead + Send + Sync + 'static,
{
    async fn call(&self, request: CallRequest, block_id: BlockId) -> RpcResult<RpcBytes> {
        self.call_impl(request, block_id).await
    }
}
