use super::{utils, ApiImpl};
use crate::api::EthApiServer;
use crate::api_impl::utils::create_txn_env;
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use alloy::rpc::types::state::StateOverride;
use alloy::sol_types::{decode_revert_reason, SolValue};
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{
    block_env_from_block, calc_next_block_base_fee, Address, BaseFeeParams, Block, BlockId,
    BlockNumberOrTag, BlockOverrides, Bytes, CallRequest, Header, Index, JsonStorageKey,
    MultiCallErrorCode, MultiCallResp, MultiCallStats, SingleCallResult, Transaction, H256, U256,
};
use leafage_evm_types::{CfgEnv, SpecId};
use revm::context::result::ExecutionResult;
use revm::database::{CacheDB, DatabaseRef};
use revm::inspector::NoOpInspector;
use revm::ExecuteEvm;
use serde_json::Value;
use std::error::Error;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::error;

impl<DB: EvmStorageRead + BlockIndex> ApiImpl<DB> {
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
        let base_fee = calc_next_block_base_fee(
            block.header.gas_used,
            block.header.gas_limit,
            block.header.base_fee_per_gas.unwrap_or_default(),
            BaseFeeParams::ethereum(),
        );
        Ok(base_fee)
    }

    async fn call_impl(
        &self,
        request: CallRequest,
        block_id: BlockId,
        state_override: Option<StateOverride>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<Bytes> {
        let cfg = self.cfg.clone();
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
        let mut block_env = block_env_from_block(&block);
        let mut db = CacheDB::new(EvmStorageWrapper {
            db: state.clone(),
            ovm_address: self.ovm_address.clone(),
        });
        let tx = create_txn_env(&block_env, request, &db, &cfg)?;
        if let Some(overrides) = block_overrides {
            super::utils::apply_block_overrides(overrides, &mut db, &mut block_env);
        }
        if let Some(state_override) = state_override {
            super::utils::apply_state_overrides(state_override, &mut db)?;
        }
        let mut evm = utils::create_evm_from_state(block_env, cfg, db, NoOpInspector {});
        let res = evm
            .transact(tx)
            .map_err(|e| internal_rpc_err(format!("{:?}", e)))?;
        match res.result {
            ExecutionResult::Success { output, .. } => Ok(output.into_data().0.into()),
            ExecutionResult::Revert { output, .. } => Err(internal_rpc_err(format!(
                "Reverted: {:?}",
                decode_revert_reason(&output).unwrap_or("Reason Unknown".to_string())
            ))
            .into()),
            ExecutionResult::Halt { reason, gas_used } => {
                Err(internal_rpc_err(format!("Halted: {:?} {}", reason, gas_used)).into())
            }
        }
    }

    fn eth_erc20_handle<StateDB>(
        block_header: &Header,
        state: StateDB,
        request: CallRequest,
    ) -> SingleCallResult
    where
        StateDB: DatabaseRef,
        StateDB::Error: Error,
    {
        if let Some(data) = request.input.input() {
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
                    result: Bytes::from(res.abi_encode()),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            } else if data[0..4] == [0x18, 0x16, 0x0d, 0xdd] {
                // totalSupply
                return SingleCallResult {
                    code: 0,
                    err: "".to_string(),
                    from_cache: false,
                    result: Bytes::from(U256::from(1u32).abi_encode()),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            } else if data[0..4] == [0x31, 0x3c, 0xe5, 0x67] {
                // decimals
                return SingleCallResult {
                    code: 0,
                    err: "".to_string(),
                    from_cache: false,
                    result: Bytes::from(U256::from(18u32).abi_encode()),
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
                    result: Bytes::from("ETH".abi_encode()),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            } else if data[0..4] == [0x6c, 0x4b, 0x6e, 0x28] {
                let block_num = U256::from(block_header.number);
                let block_hash = block_header.hash;
                return SingleCallResult {
                    code: 0,
                    err: "".to_string(),
                    from_cache: false,
                    result: Bytes::from((block_num, block_hash).abi_encode()),
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
        fast_fail: Option<bool>,
        _use_parallel: Option<bool>,
        _disable_cache: Option<bool>,
    ) -> RpcResult<MultiCallResp> {
        let cfg = self.cfg.clone();
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
        let ovm_address = self.ovm_address.clone();
        tokio::task::spawn_blocking(move || {
            let rsp = Self::multi_call_from_state(
                requests,
                cfg,
                EvmStorageWrapper {
                    db: state,
                    ovm_address,
                },
                block,
                fast_fail.unwrap_or_default(),
            );
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
        cfg: CfgEnv<SpecId>,
        state: EvmStorageWrapper<<DB as EvmStorageRead>::StateDB>,
        block: Arc<Block<Transaction>>,
        fast_fail: bool,
    ) -> RpcResult<MultiCallResp> {
        let block_env = block_env_from_block(&block);
        let mut stats = MultiCallStats {
            block_num: block.header.number,
            block_time: block.header.timestamp,
            block_hash: block.header.hash,
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
            if let Some(txkind) = request.to {
                if let Some(address) = txkind.to() {
                    if *address
                        == Address::from_str("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee").unwrap()
                    {
                        let mut res = Self::eth_erc20_handle(&block.header, state.clone(), request);
                        if res.code != MultiCallErrorCode::Success as i32 {
                            stats.success = false;
                        }
                        res.time_cost = start.elapsed().as_secs_f64();
                        results.push(res);
                        continue;
                    }
                }
            }
            let tx = create_txn_env(&block_env, request, &state, &cfg)?;
            let mut evm = utils::create_evm_from_state(
                block_env.clone(),
                cfg.clone(),
                state.clone(),
                NoOpInspector {},
            );

            let res = evm
                .transact(tx)
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
                    err: decode_revert_reason(&output).unwrap_or("Reason Unknown".to_string()),
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
            .state_at(BlockId::Number(BlockNumberOrTag::Latest))
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        let state = state.unwrap();
        let block = state
            .block_info_arc()
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        Ok(U256::from(block.header.number))
    }

    fn get_balance_impl(&self, address: Address, block_id: BlockId) -> RpcResult<U256> {
        let state = self
            .db
            .state_at(block_id)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if state.is_none() {
            return Err(invalid_params_rpc_err("Block not found".to_string()));
        }
        Self::get_balance_from_state(
            EvmStorageWrapper {
                db: state.unwrap(),
                ovm_address: self.ovm_address.clone(),
            },
            address,
        )
        .map_err(|e| internal_rpc_err(e.to_string()))
    }

    pub fn get_balance_from_state<StateDB>(state: StateDB, address: Address) -> RpcResult<U256>
    where
        StateDB: DatabaseRef,
        StateDB::Error: Error,
    {
        let account = state
            .basic_ref(address.0.into())
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let balance = account.map(|a| a.balance);
        Ok(balance.unwrap_or_default().into())
    }

    fn get_block_by_id_impl(&self, block_id: BlockId, full: bool) -> RpcResult<Option<Value>> {
        let block = self
            .db
            .get_block_by_id_arc(block_id)
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if block.is_none() {
            return Ok(None);
        }
        let block = block.unwrap();
        let value = if !full {
            let mut block: Block<Transaction> = block.as_ref().clone().into();
            block.transactions.convert_to_hashes();
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
        let state = EvmStorageWrapper {
            db: state.unwrap(),
            ovm_address: self.ovm_address.clone(),
        };
        let account = state
            .basic_ref(address.0.into())
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if account.is_none() {
            return Ok(Bytes::new());
        } else {
            let account = account.unwrap();
            if account.code_hash.is_zero() {
                return Ok(Bytes::new());
            }
            let code = state
                .code_by_hash_ref(account.code_hash)
                .map_err(|e| internal_rpc_err(e.to_string()))?;
            Ok(code.original_bytes().0.clone().into())
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
        let state = EvmStorageWrapper {
            db: state.unwrap(),
            ovm_address: self.ovm_address.clone(),
        };
        let storage = state
            .storage_ref(address.0.into(), U256::from_be_bytes(index.into()))
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
        let state = EvmStorageWrapper {
            db: state.unwrap(),
            ovm_address: self.ovm_address.clone(),
        };
        let account = state
            .basic_ref(address.0.into())
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let nonce = account.map(|a| a.nonce);
        Ok(U256::from(nonce.unwrap_or_default()))
    }

    fn chain_id_impl(&self) -> RpcResult<U256> {
        Ok(U256::from(self.cfg.chain_id))
    }

    async fn transaction_by_block_hash_and_index_impl(
        &self,
        hash: H256,
        index: Index,
    ) -> RpcResult<Option<Transaction>> {
        let block = self
            .db
            .get_block_by_id_arc(hash.into())
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        if block.is_none() {
            return Ok(None);
        }
        let block = block.unwrap();
        let txns = block.transactions.as_transactions();
        if txns.is_none() {
            return Ok(None);
        }
        let txns = txns.unwrap();
        if index.0 >= txns.len() {
            return Ok(None);
        }
        Ok(Some(txns[index.0].clone()))
    }
}

#[async_trait::async_trait]
impl<DB> EthApiServer for ApiImpl<DB>
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
        self.call_impl(request, block_id, state_override, block_overrides)
            .await
    }

    async fn multi_call(
        &self,
        requests: Vec<CallRequest>,
        block_id: BlockId,
        fast_fail: Option<bool>,
        use_parallel: Option<bool>,
        disable_cache: Option<bool>,
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
        block_number: BlockNumberOrTag,
        full: bool,
    ) -> RpcResult<Option<Value>> {
        self.get_block_by_id_impl(BlockId::Number(block_number), full)
    }

    async fn get_block_by_hash(&self, block_hash: H256, full: bool) -> RpcResult<Option<Value>> {
        self.get_block_by_id_impl(BlockId::Hash(block_hash.into()), full)
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
        self.get_storage_at_impl(address, position.as_b256(), block_number)
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
        self.base_fee_impl(block_number.unwrap_or(BlockId::Number(BlockNumberOrTag::Latest)))
            .await
    }

    async fn transaction_by_block_hash_and_index(
        &self,
        hash: H256,
        index: Index,
    ) -> RpcResult<Option<Transaction>> {
        self.transaction_by_block_hash_and_index_impl(hash, index)
            .await
    }
}
