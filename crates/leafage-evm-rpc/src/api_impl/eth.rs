use crate::api::EthApiServer;
use crate::api_impl::utils::{create_txn_env, decode_revert_reason};
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use ethers_core::abi::AbiEncode;
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockContext, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{
    block_env_from_block, calculate_next_block_base_fee, Address, BaseFeeParams, Block, BlockId,
    BlockNumber, Bytes, CallRequest, JsonStorageKey, MultiCallErrorCode, MultiCallResp,
    MultiCallStats, SingleCallResult, Transaction, TxHash, H256, RU256, U256,
};
use revm::db::DatabaseRef;
use revm::primitives::{CfgEnv, Env, ExecutionResult};
use revm::EVM;
use serde_json::Value;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::error;

/// [`EthApiImpl`] implements the EthApi trait.
pub struct EthApiImpl<DB> {
    db: DB,
    cfg: CfgEnv,
}

impl<DB: EvmStorageRead> EthApiImpl<DB> {
    pub fn new(db: DB, cfg: CfgEnv) -> Self {
        Self { db, cfg }
    }

    async fn base_fee_impl(&self, block_id: BlockId) -> RpcResult<u64> {
        let state = self
            .db
            .state_at(block_id)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let block = state
            .unwrap()
            .block_info_arc()
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let base_fee = calculate_next_block_base_fee(
            block.gas_used.as_u64(),
            block.gas_limit.as_u64(),
            block.base_fee_per_gas.unwrap_or_default().as_u64(),
            BaseFeeParams::ethereum(),
        );
        Ok(base_fee)
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
            .block_info_arc()
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let block_env = block_env_from_block(&block);
        let tx = create_txn_env(&block_env, request)?;
        let env = Env {
            block: block_env,
            cfg,
            tx,
        };
        let mut evm = EVM::with_env(env);
        evm.database(EvmStorageWrapper(state));
        let res = evm
            .transact_ref()
            .map_err(|e| internal_rpc_err(format!("{:?}", e)))?;
        match res.result {
            ExecutionResult::Success { output, .. } => Ok(output.into_data().0.into()),
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

    fn eth_erc20_handle(state: DB::StateDB, request: CallRequest) -> SingleCallResult {
        if let Some(data) = request.data {
            if data.len() < 4 {
                return SingleCallResult {
                    code: MultiCallErrorCode::CodeTxArgs as i32, // tx arg error
                    err: "tx input less than 4 bytes".to_string(),
                    from_cache: false,
                    result: Default::default(),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            }
            // balanceOf
            if data[0..4] == [0x70, 0xa0, 0x82, 0x31] {
                // 4(selector) + 32(user addr)
                if data.len() < 36 {
                    return SingleCallResult {
                        code: MultiCallErrorCode::CodeTxArgs as i32, // tx arg error
                        err: "".to_string(),
                        from_cache: false,
                        result: Default::default(),
                        gas_used: 0,
                        time_cost: 0.0,
                    };
                }

                let mut h160_bytes = [0u8; 20];
                h160_bytes.copy_from_slice(&data[16..]);
                let user_addr = Address::from(h160_bytes);

                // get address's native balance
                let res = Self::get_balance_from_state(state, user_addr)
                    .map(|u256| u256)
                    .unwrap_or_default();

                return SingleCallResult {
                    code: MultiCallErrorCode::Success as i32,
                    err: "".to_string(),
                    from_cache: false,
                    result: Bytes::from(res.encode()),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            } else if data[0..4] == [0x18, 0x16, 0x0d, 0xdd] {
                // totalSupply
                return SingleCallResult {
                    code: 0,
                    err: "".to_string(),
                    from_cache: false,
                    result: Bytes::from(U256::from(1u32).encode()),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            } else if data[0..4] == [0x31, 0x3c, 0xe5, 0x67] {
                // decimals
                return SingleCallResult {
                    code: 0,
                    err: "".to_string(),
                    from_cache: false,
                    result: Bytes::from(U256::from(18u32).encode()),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            } else if data[0..4] == [0x06, 0xfd, 0xde, 0x03]
                || data[0..4] == [0x95, 0xd8, 0x9b, 0x41]
            {
                // name, symbol. abi encoded of the string "ETH"
                return SingleCallResult {
                    code: 0,
                    err: "".to_string(),
                    from_cache: false,
                    result: Bytes::from("ETH".encode()),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            } else {
                return SingleCallResult {
                    code: MultiCallErrorCode::NativeMethodNotFound as i32,
                    err: "method not found".to_string(),
                    from_cache: false,
                    result: Default::default(),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            }
        } else {
            return SingleCallResult {
                code: MultiCallErrorCode::CodeTxArgs as i32, // tx arg error
                err: "tx input missing".to_string(),
                from_cache: false,
                result: Bytes::default(),
                gas_used: 0,
                time_cost: 0.0,
            };
        }
    }

    async fn multi_call_impl(
        &self,
        requests: Vec<CallRequest>,
        block_id: BlockId,
        fast_fail: bool,
        _use_parallel: bool,
        _disable_cache: bool,
    ) -> RpcResult<MultiCallResp> {
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
            .block_info_arc()
            .map_err(|e| internal_rpc_err(e.to_string()))?;

        let (tx, rx) = oneshot::channel();

        tokio::task::spawn_blocking(move || {
            let rsp = Self::multi_call_from_state(requests, cfg, state, block, fast_fail);
            if let Err(e) = tx.send(rsp) {
                error!("Failed to send multi_call result: {:?}", e);
            }
        });
        let rsp = rx
            .await
            .map_err(|_| internal_rpc_err("MultiCall failed".to_string()))?;
        rsp
    }

    fn multi_call_from_state(
        requests: Vec<CallRequest>,
        cfg: CfgEnv,
        state: DB::StateDB,
        block: Arc<Block<Transaction>>,
        fast_fail: bool,
    ) -> RpcResult<MultiCallResp> {
        let block_env = block_env_from_block(&block);
        let mut stats = MultiCallStats {
            block_num: block.number.unwrap().as_u64(),
            block_time: block.timestamp.as_u64(),
            block_hash: block.hash.unwrap(),
            success: true,
            cache_enabled: false,
        };
        // run in sequence
        let mut results: Vec<SingleCallResult> = vec![];
        for request in requests {
            let start = std::time::Instant::now();
            if fast_fail
                && !results.is_empty()
                && results.last().unwrap().code != MultiCallErrorCode::Success as i32
            {
                let mut res = results.last().unwrap().clone();
                res.code = MultiCallErrorCode::EVMFastFailed as i32;
                results.push(res);
                continue;
            }
            if let Some(addres) = request.to {
                if addres == Address::from_str("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee").unwrap()
                {
                    let mut res = Self::eth_erc20_handle(state.clone(), request);
                    if res.code != MultiCallErrorCode::Success as i32 {
                        stats.success = false;
                    }
                    res.time_cost = start.elapsed().as_secs_f64();
                    results.push(res);
                    continue;
                }
            }
            let tx = create_txn_env(&block_env, request)?;
            let env = Env {
                block: block_env.clone(),
                cfg: cfg.clone(),
                tx,
            };
            let mut evm = EVM::with_env(env);
            evm.database(EvmStorageWrapper(state.clone()));
            let res = evm
                .transact_ref()
                .map_err(|e| internal_rpc_err(format!("{:?}", e)))?;
            let mut res = match res.result {
                ExecutionResult::Success {
                    output, gas_used, ..
                } => SingleCallResult {
                    code: MultiCallErrorCode::Success as i32,
                    err: "".to_string(),
                    from_cache: false,
                    result: output.into_data().0.into(),
                    gas_used: gas_used as i64,
                    time_cost: 0.0,
                },
                ExecutionResult::Revert {
                    output, gas_used, ..
                } => SingleCallResult {
                    code: MultiCallErrorCode::EVMReverted as i32,
                    err: decode_revert_reason(output).unwrap_or("Reason Unknown".to_string()),
                    from_cache: false,
                    result: Bytes::default(),
                    gas_used: gas_used as i64,
                    time_cost: 0.0,
                },
                ExecutionResult::Halt { reason, gas_used } => SingleCallResult {
                    code: MultiCallErrorCode::EVMCancelled as i32,
                    err: format!("Halted: {:?}", reason),
                    from_cache: false,
                    result: Bytes::default(),
                    gas_used: gas_used as i64,
                    time_cost: 0.0,
                },
            };
            res.time_cost = start.elapsed().as_secs_f64();
            if res.code != MultiCallErrorCode::Success as i32 {
                stats.success = false;
            }
            results.push(res);
        }

        let rsp = MultiCallResp { results, stats };
        Ok(rsp)
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
        Self::get_balance_from_state(state.unwrap(), address)
    }

    fn get_balance_from_state(state: DB::StateDB, address: Address) -> RpcResult<U256> {
        let state = EvmStorageWrapper(state);
        let account = state
            .basic(address.0.into())
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
            .basic(address.0.into())
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
            Ok(code.bytecode.0.into())
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
            .storage(address.0.into(), RU256::from_be_bytes(index.into()))
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
            .basic(address.0.into())
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

    async fn multi_call(
        &self,
        requests: Vec<CallRequest>,
        block_id: BlockId,
        fast_fail: bool,
        use_parallel: bool,
        disable_cache: bool,
    ) -> RpcResult<MultiCallResp> {
        self.multi_call_impl(requests, block_id, fast_fail, use_parallel, disable_cache)
            .await
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

    async fn base_fee(&self, block_number: Option<BlockId>) -> RpcResult<u64> {
        self.base_fee_impl(block_number.unwrap_or(BlockId::Number(BlockNumber::Latest)))
            .await
    }
}
