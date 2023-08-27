use crate::api::EthApiServer;
use crate::api_impl::utils::{create_txn_env, decode_revert_reason};
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockContext, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{
    block_env_from_block, Address, Block, BlockId, BlockNumber, Bytes, CallRequest, JsonStorageKey,
    TxHash, H256, RU256, U256,
};
use revm::db::DatabaseRef;
use revm::primitives::{CfgEnv, Env, ExecutionResult};
use revm::EVM;
use serde_json::Value;

pub struct EthApiImpl<DB> {
    db: DB,
    cfg: CfgEnv,
}

impl<DB: EvmStorageRead> EthApiImpl<DB> {
    pub fn new(db: DB, cfg: CfgEnv) -> Self {
        Self { db, cfg }
    }

    async fn call_impl(&self, request: CallRequest, block_id: BlockId) -> RpcResult<Bytes> {
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
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let block_env = block_env_from_block(&block);
        let tx = create_txn_env(&block_env, request)?;
        let env = Env {
            block: block_env,
            cfg,
            tx,
        };
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

    fn block_number_impl(&self) -> RpcResult<U256> {
        let state = self
            .db
            .state_at(BlockId::Number(BlockNumber::Latest))
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = state.unwrap();
        let block = state
            .block_info_arc()
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        Ok(block.number.unwrap().as_u64().into())
    }

    fn get_balance_impl(&self, address: Address, block_id: BlockId) -> RpcResult<U256> {
        let state = self
            .db
            .state_at(block_id)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = EvmStorageWrapper(state.unwrap());
        let account = state
            .basic(address.into())
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let balance = account.map(|a| a.balance);
        Ok(balance.unwrap_or_default().into())
    }

    fn get_block_by_id_impl(&self, block_number: BlockId, full: bool) -> RpcResult<Option<Value>> {
        let state = self
            .db
            .state_at(block_number)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Ok(None);
        }
        let state = state.unwrap();
        let block = state
            .block_info_arc()
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let value = if !full {
            let block: Block<TxHash> = block.as_ref().clone().into();
            serde_json::to_value(block).map_err(|e| internal_rpc_err(e.to_string()))?
        } else {
            serde_json::to_value(block).map_err(|e| internal_rpc_err(e.to_string()))?
        };
        Ok(Some(value))
    }

    fn get_code_impl(&self, address: Address, block_number: BlockId) -> RpcResult<Bytes> {
        let state = self
            .db
            .state_at(block_number)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = EvmStorageWrapper(state.unwrap());
        let account = state
            .basic(address.into())
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if account.is_none() {
            return Ok(Bytes::new());
        } else {
            let account = account.unwrap();
            if account.code_hash.is_zero() {
                return Ok(Bytes::new());
            }
            let code = state
                .code_by_hash(account.code_hash)
                .map_err(|e| internal_rpc_err(e.to_string()))?;
            Ok(code.bytecode.into())
        }
    }

    fn get_storage_at_impl(
        &self,
        address: Address,
        index: H256,
        block_number: BlockId,
    ) -> RpcResult<H256> {
        let state = self
            .db
            .state_at(block_number)
            .map_err(|e| internal_rpc_err(e.to_string()))?;

        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = EvmStorageWrapper(state.unwrap());
        let storage = state
            .storage(address.into(), RU256::from_be_bytes(index.into()))
            .map_err(|e| {
                internal_rpc_err(format!(
                    "Failed to get storage at {:?} {:?}: {:?}",
                    address, index, e
                ))
            })?;
        let value: [u8; 32] = storage.to_be_bytes();
        Ok(value.into())
    }

    fn get_transaction_count_impl(
        &self,
        address: Address,
        block_number: BlockId,
    ) -> RpcResult<U256> {
        let state = self
            .db
            .state_at(block_number)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = EvmStorageWrapper(state.unwrap());
        let account = state
            .basic(address.into())
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let nonce = account.map(|a| a.nonce);
        Ok(nonce.unwrap_or_default().into())
    }

    fn chain_id_impl(&self) -> RpcResult<U256> {
        Ok(self.cfg.chain_id.into())
    }
}

#[async_trait::async_trait]
impl<DB> EthApiServer for EthApiImpl<DB>
where
    DB: EvmStorageRead + Send + Sync + 'static,
{
    async fn call(&self, request: CallRequest, block_id: BlockId) -> RpcResult<Bytes> {
        self.call_impl(request, block_id).await
    }

    async fn block_number(&self) -> RpcResult<U256> {
        self.block_number_impl()
    }

    async fn get_balance(&self, address: Address, block_id: BlockId) -> RpcResult<U256> {
        self.get_balance_impl(address, block_id)
    }

    async fn get_block_by_number(
        &self,
        block_number: BlockNumber,
        full: bool,
    ) -> RpcResult<Option<Value>> {
        self.get_block_by_id_impl(BlockId::Number(block_number), full)
    }

    async fn get_block_by_hash(&self, block_hash: H256, full: bool) -> RpcResult<Option<Value>> {
        self.get_block_by_id_impl(BlockId::Hash(block_hash), full)
    }

    async fn get_code(&self, address: Address, block_number: BlockId) -> RpcResult<Bytes> {
        self.get_code_impl(address, block_number)
    }

    async fn get_storage_at(
        &self,
        address: Address,
        position: JsonStorageKey,
        block_number: BlockId,
    ) -> RpcResult<H256> {
        self.get_storage_at_impl(address, position.0, block_number)
    }

    async fn get_transaction_count(
        &self,
        address: Address,
        block_number: BlockId,
    ) -> RpcResult<U256> {
        self.get_transaction_count_impl(address, block_number)
    }

    async fn chain_id(&self) -> RpcResult<U256> {
        self.chain_id_impl()
    }
}
