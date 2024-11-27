use super::ApiImpl;
use crate::api::DebankApiServer;
use crate::api_impl::utils::{create_txn_env, get_handler_cfg};
use crate::error::{internal_rpc_err, rpc_error_with_code, DebankErrorCode};
use alloy::sol_types::{decode_revert_reason, SolValue};
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{
    BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrapper, TransactionIndex,
};
use leafage_evm_types::{
    block_env_from_block, Address, Block, BlockId, BlockNumberOrTag, BlockOverrides, BlockType,
    Bytes, CallRequest, DebankBlock, DebankBlockContext, DebankMultiCallResp, DebankMultiCallStats,
    DebankSimulateResp, DebankSingleCallResult, HaltReason, MultiCallErrorCode, Transaction, H256,
    U256,
};
use revm::db::{CacheDB, DatabaseRef};
use revm::primitives::{CfgEnv, EnvWithHandlerCfg, ExecutionResult, SpecId};
use revm::Evm;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::error;

impl<DB: EvmStorageRead + BlockIndex + TransactionIndex> ApiImpl<DB> {
    fn debank_get_state_by_ctx_impl(
        &self,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<<DB as EvmStorageRead>::StateDB> {
        if block_ctx.is_none() {
            let state = self
                .db
                .state_at(BlockId::Number(BlockNumberOrTag::Latest))
                .map_err(|e| {
                    rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
                })?;
            return Ok(state.unwrap());
        }

        let block_ctx = block_ctx.unwrap();

        let mut state = self.db.state_at(block_ctx.block_id).map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;
        if state.is_none() {
            if block_ctx.block_type == BlockType::Equals {
                return Err(rpc_error_with_code(
                    DebankErrorCode::BlockNotFound as i32,
                    "block not found".to_string(),
                ));
            } else {
                state = self
                    .db
                    .state_at(BlockId::Number(BlockNumberOrTag::Latest))
                    .map_err(|e| {
                        rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
                    })?;
            }
        }
        let state = state.unwrap();
        Ok(state)
    }

    fn debank_get_latest_block_impl(&self) -> RpcResult<DebankBlock> {
        let block = self
            .db
            .get_block_by_id_arc(BlockId::Number(BlockNumberOrTag::Latest))
            .map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;

        let block = block.unwrap();
        Ok(DebankBlock {
            block_id: block.header.hash,
            block_height: block.header.number,
            block_timestamp: block.header.timestamp,
        })
    }

    fn debank_get_block_by_height_impl(&self, height: U256) -> RpcResult<DebankBlock> {
        let number: u64 = height.try_into().map_err(|_| {
            rpc_error_with_code(
                DebankErrorCode::InvalidParams as i32,
                "block height out of range".to_string(),
            )
        })?;
        let block = self
            .db
            .get_block_by_id_arc(BlockId::Number(BlockNumberOrTag::Number(number)))
            .map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
        if block.is_none() {
            return Err(rpc_error_with_code(
                DebankErrorCode::BlockNotFound as i32,
                "block not found".to_string(),
            ));
        }

        let block = block.unwrap();
        Ok(DebankBlock {
            block_id: block.header.hash,
            block_height: block.header.number,
            block_timestamp: block.header.timestamp,
        })
    }

    fn debank_get_block_by_id_impl(&self, id: H256) -> RpcResult<DebankBlock> {
        let block = self
            .db
            .get_block_by_id_arc(BlockId::Hash(id.into()))
            .map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
        if block.is_none() {
            return Err(rpc_error_with_code(
                DebankErrorCode::BlockNotFound as i32,
                "block not found".to_string(),
            ));
        }
        let block = block.unwrap();
        Ok(DebankBlock {
            block_id: block.header.hash,
            block_height: block.header.number,
            block_timestamp: block.header.timestamp,
        })
    }

    fn debank_get_address_nonce_impl(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<U256> {
        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let state = EvmStorageWrapper(state);
        let account = state.basic_ref(address.0.into()).map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;
        let nonce = account.map(|a| a.nonce);
        Ok(U256::from(nonce.unwrap_or_default()))
    }

    fn debank_get_address_balance_impl(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<U256> {
        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let state = EvmStorageWrapper(state);
        let account = state.basic_ref(address.0.into()).map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;
        let balance = account.map(|a| a.balance);
        Ok(U256::from(balance.unwrap_or_default()))
    }

    fn debank_get_code_impl(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<Bytes> {
        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let state = EvmStorageWrapper(state);
        let account = state.basic_ref(address.0.into()).map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;
        if account.is_none() {
            return Ok(Bytes::new());
        } else {
            let account = account.unwrap();
            if account.code_hash.is_zero() {
                return Ok(Bytes::new());
            }
            let code = state.code_by_hash_ref(account.code_hash).map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
            Ok(code.bytecode().0.clone().into())
        }
    }

    async fn contract_multi_call_impl(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
        fast_fail: Option<bool>,
        _use_parallel: Option<bool>,
        _disable_cache: Option<bool>,
    ) -> RpcResult<DebankMultiCallResp> {
        let mut cfg = self.cfg.clone();
        cfg.disable_eip3607 = true;
        cfg.disable_base_fee = true;
        cfg.disable_block_gas_limit = true;

        let spec_id = self.spec_id;

        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let block = state.block_info_arc().map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;

        let (tx, rx) = oneshot::channel();

        tokio::task::spawn_blocking(move || {
            let rsp = Self::debank_multi_call_from_state(
                requests,
                cfg,
                spec_id,
                state,
                block,
                block_overrides,
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

    fn debank_multi_call_from_state(
        requests: Vec<CallRequest>,
        cfg: CfgEnv,
        spec_id: SpecId,
        state: DB::StateDB,
        block: Arc<Block<Transaction>>,
        block_overrides: Option<BlockOverrides>,
        fast_fail: bool,
    ) -> RpcResult<DebankMultiCallResp> {
        let block_env = block_env_from_block(&block);
        let mut stats = DebankMultiCallStats {
            block_num: block.header.number,
            block_time: block.header.timestamp,
            block_hash: block.header.hash,
            success: true,
            cache_enabled: false,
        };
        // run in sequence
        let mut results: Vec<DebankSingleCallResult> = vec![];
        for request in requests {
            let start = std::time::Instant::now();
            if fast_fail && !results.is_empty() && results.last().unwrap().code != 0 {
                let res = results.last().unwrap().clone();
                results.push(res);
                continue;
            }
            if let Some(txkind) = request.to {
                if let Some(address) = txkind.to() {
                    if *address
                        == Address::from_str("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee").unwrap()
                    {
                        let mut res = Self::debank_eth_erc20_handle(state.clone(), request);
                        if res.code != 0 as i32 {
                            stats.success = false;
                        }
                        res.time_cost = start.elapsed().as_secs_f64();
                        results.push(res);
                        continue;
                    }
                }
            }
            let cfg = get_handler_cfg(cfg.clone(), spec_id);
            let tx = create_txn_env(&block_env, request)?;
            let mut env = EnvWithHandlerCfg::new_with_cfg_env(cfg, block_env.clone(), tx);
            let mut db = CacheDB::new(EvmStorageWrapper(state.clone()));
            if let Some(overrides) = block_overrides.clone() {
                super::utils::apply_block_overrides(overrides, &mut db, &mut env.block);
            }
            let mut evm = Evm::builder()
                .with_ref_db(db)
                .with_env_with_handler_cfg(env)
                .build();
            let res = evm.transact().map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
            let mut res = match res.result {
                ExecutionResult::Success {
                    output, gas_used, ..
                } => DebankSingleCallResult {
                    code: 0,
                    err: "".to_string(),
                    from_cache: false,
                    result: output.into_data().0.into(),
                    gas_used: gas_used as i64,
                    time_cost: 0.0,
                },
                ExecutionResult::Revert {
                    output, gas_used, ..
                } => DebankSingleCallResult {
                    code: DebankErrorCode::EvmRevert as i32,
                    err: decode_revert_reason(&output).unwrap_or("Reason Unknown".to_string()),
                    from_cache: false,
                    result: Bytes::default(),
                    gas_used: gas_used as i64,
                    time_cost: 0.0,
                },
                ExecutionResult::Halt { reason, gas_used } => DebankSingleCallResult {
                    code: match reason {
                        HaltReason::OutOfFunds => DebankErrorCode::BalanceExhausted as i32,
                        HaltReason::OutOfGas(_) => DebankErrorCode::GasExhausted as i32,
                        HaltReason::NonceOverflow => DebankErrorCode::NonceError as i32,
                        _ => DebankErrorCode::EvmFailed as i32,
                    },
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
        Ok(DebankMultiCallResp { stats, results })
    }

    fn debank_eth_erc20_handle(state: DB::StateDB, request: CallRequest) -> DebankSingleCallResult {
        if let Some(data) = request.input.input() {
            if data.len() < 4 {
                return DebankSingleCallResult {
                    code: DebankErrorCode::InvalidParams as i32, // tx arg error
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
                    return DebankSingleCallResult {
                        code: DebankErrorCode::InvalidParams as i32, // tx arg error
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

                return DebankSingleCallResult {
                    code: 0,
                    err: "".to_string(),
                    from_cache: false,
                    result: Bytes::from(res.abi_encode()),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            } else if data[0..4] == [0x18, 0x16, 0x0d, 0xdd] {
                // totalSupply
                return DebankSingleCallResult {
                    code: 0,
                    err: "".to_string(),
                    from_cache: false,
                    result: Bytes::from(U256::from(1u32).abi_encode()),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            } else if data[0..4] == [0x31, 0x3c, 0xe5, 0x67] {
                // decimals
                return DebankSingleCallResult {
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
                return DebankSingleCallResult {
                    code: 0,
                    err: "".to_string(),
                    from_cache: false,
                    result: Bytes::from("ETH".abi_encode()),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            } else {
                return DebankSingleCallResult {
                    code: DebankErrorCode::MethodNotFound as i32,
                    err: "method not found".to_string(),
                    from_cache: false,
                    result: Default::default(),
                    gas_used: 0,
                    time_cost: 0.0,
                };
            }
        } else {
            return DebankSingleCallResult {
                code: DebankErrorCode::InvalidParams as i32, // tx arg error
                err: "tx input missing".to_string(),
                from_cache: false,
                result: Bytes::default(),
                gas_used: 0,
                time_cost: 0.0,
            };
        }
    }

    fn block_is_valid_impl(&self, id: H256) -> RpcResult<bool> {
        let block = self
            .db
            .get_block_by_id_arc(BlockId::Hash(id.into()))
            .map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
        if block.is_none() {
            return Err(rpc_error_with_code(
                DebankErrorCode::BlockNotFound as i32,
                "block not found".to_string(),
            ));
        }

        let block = block.unwrap();
        let canonical_block = self
            .db
            .get_block_by_id(BlockId::Number(BlockNumberOrTag::Number(
                block.header.number,
            )))
            .map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
        if canonical_block.is_none() {
            return Ok(false);
        }
        Ok(block.header.hash == canonical_block.unwrap().header.hash)
    }
}

#[async_trait::async_trait]
impl<DB: EvmStorageRead + BlockIndex + TransactionIndex + Send + Sync + 'static> DebankApiServer
    for ApiImpl<DB>
{
    async fn get_address_nonce(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<U256> {
        self.debank_get_address_nonce_impl(address, block_ctx)
    }

    async fn get_address_balance(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<U256> {
        self.debank_get_address_balance_impl(address, block_ctx)
    }

    async fn get_address_code(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<Bytes> {
        self.debank_get_code_impl(address, block_ctx)
    }

    async fn contract_multi_call(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
        fast_fail: Option<bool>,
        use_parallel: Option<bool>,
        disable_cache: Option<bool>,
    ) -> RpcResult<DebankMultiCallResp> {
        self.contract_multi_call_impl(
            requests,
            block_ctx,
            block_overrides,
            fast_fail,
            use_parallel,
            disable_cache,
        )
        .await
    }

    async fn simulate_transactions(
        &self,
        _requests: Vec<CallRequest>,
        _block_ctx: Option<DebankBlockContext>,
        _block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<DebankSimulateResp> {
        Ok(DebankSimulateResp::default())
    }

    async fn get_latest_block(&self) -> RpcResult<DebankBlock> {
        self.debank_get_latest_block_impl()
    }

    async fn get_block_by_height(&self, height: U256) -> RpcResult<DebankBlock> {
        self.debank_get_block_by_height_impl(height)
    }

    async fn get_block_by_id(&self, id: H256) -> RpcResult<DebankBlock> {
        self.debank_get_block_by_id_impl(id)
    }

    async fn block_is_valid(&self, id: H256) -> RpcResult<bool> {
        self.block_is_valid_impl(id)
    }
}
