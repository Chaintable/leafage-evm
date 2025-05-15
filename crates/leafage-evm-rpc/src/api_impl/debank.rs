use super::{utils, ApiImpl};
use crate::api::DebankApiServer;
use crate::api_impl::utils::{build_debank_traces, create_txn_env};
use crate::error::{internal_rpc_err, rpc_error_with_code, DebankErrorCode};
use alloy::rpc::types::state::StateOverride;
use alloy::sol_types::{decode_revert_reason, SolValue};
use jsonrpsee::core::RpcResult;
use leafage_evm_storage::{BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrapper, StateDB};
use leafage_evm_types::{
    block_env_from_block, Address, Block, BlockId, BlockNumberOrTag, BlockOverrides, BlockType,
    Bytes, CallRequest, DebankBlock, DebankBlockContext, DebankMultiCallResp, DebankMultiCallStats,
    DebankSimulateResp, DebankSimulateStats, DebankSingleCallResult, DebankSingleSimulateResult,
    JsonStorageKey, MultiCallErrorCode, Transaction, TransactionInfo, H256, KECCAK256_EMPTY, U256,
};
use leafage_evm_types::{CfgEnv, SpecId};
#[cfg(feature = "optimism")]
use op_revm::OpTransactionError;
use revm::context::result::{EVMError, InvalidTransaction};
use revm::context::result::{ExecutionResult, HaltReason};
use revm::context::{TransactTo, Transaction as TransactionTrait};
use revm::database::{CacheDB, DatabaseRef};
use revm::inspector::NoOpInspector;
use revm::{ExecuteCommitEvm, ExecuteEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::str::FromStr;
use std::sync::Arc;
use tokio::{sync::oneshot, time::timeout};
use tracing::error;

pub const MIN_TRANSACTION_GAS: u64 = 21_000u64;

pub const CALL_STIPEND_GAS: u64 = 2_300;

pub const ESTIMATE_GAS_ERROR_RATIO: f64 = 0.015;

impl<DB: EvmStorageRead + BlockIndex> ApiImpl<DB> {
    #[cfg(not(feature = "optimism"))]
    pub fn evm_to_debank_error(
        res: EVMError<<<DB as EvmStorageRead>::StateDB as StateDB>::Error>,
    ) -> jsonrpsee::types::ErrorObjectOwned {
        match res {
            e => match e {
                EVMError::Database(e) => {
                    rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
                }
                EVMError::Header(e) => {
                    rpc_error_with_code(DebankErrorCode::InvalidParams as i32, e.to_string())
                }
                EVMError::Transaction(InvalidTransaction::LackOfFundForMaxFee { .. }) => {
                    rpc_error_with_code(
                        DebankErrorCode::BalanceExhausted as i32,
                        "Insufficient funds".to_string(),
                    )
                }
                EVMError::Transaction(InvalidTransaction::CallerGasLimitMoreThanBlock) => {
                    rpc_error_with_code(
                        DebankErrorCode::InvalidParams as i32,
                        "Caller gas limit more than block".to_string(),
                    )
                }
                EVMError::Transaction(InvalidTransaction::CallGasCostMoreThanGasLimit {
                    ..
                }) => rpc_error_with_code(
                    DebankErrorCode::GasExhausted as i32,
                    "Invalid gas limit".to_string(),
                ),
                EVMError::Transaction(
                    InvalidTransaction::NonceOverflowInTransaction
                    | InvalidTransaction::NonceTooHigh { .. }
                    | InvalidTransaction::NonceTooLow { .. },
                ) => rpc_error_with_code(
                    DebankErrorCode::NonceError as i32,
                    "Invalid nonce".to_string(),
                ),
                e => rpc_error_with_code(DebankErrorCode::EvmFailed as i32, e.to_string()),
            },
        }
    }

    #[cfg(feature = "optimism")]
    pub fn evm_to_debank_error(
        res: EVMError<<<DB as EvmStorageRead>::StateDB as StateDB>::Error, OpTransactionError>,
    ) -> jsonrpsee::types::ErrorObjectOwned {
        match res {
            e => match e {
                EVMError::Database(e) => {
                    rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
                }
                EVMError::Header(e) => {
                    rpc_error_with_code(DebankErrorCode::InvalidParams as i32, e.to_string())
                }
                EVMError::Transaction(OpTransactionError::Base(
                    InvalidTransaction::LackOfFundForMaxFee { .. },
                )) => rpc_error_with_code(
                    DebankErrorCode::BalanceExhausted as i32,
                    "Insufficient funds".to_string(),
                ),
                EVMError::Transaction(OpTransactionError::Base(
                    InvalidTransaction::CallerGasLimitMoreThanBlock,
                )) => rpc_error_with_code(
                    DebankErrorCode::InvalidParams as i32,
                    "Caller gas limit more than block".to_string(),
                ),
                EVMError::Transaction(OpTransactionError::Base(
                    InvalidTransaction::CallGasCostMoreThanGasLimit { .. },
                )) => rpc_error_with_code(
                    DebankErrorCode::GasExhausted as i32,
                    "Invalid gas limit".to_string(),
                ),
                EVMError::Transaction(
                    OpTransactionError::Base(InvalidTransaction::NonceOverflowInTransaction)
                    | OpTransactionError::Base(InvalidTransaction::NonceTooHigh { .. })
                    | OpTransactionError::Base(InvalidTransaction::NonceTooLow { .. }),
                ) => rpc_error_with_code(
                    DebankErrorCode::NonceError as i32,
                    "Invalid nonce".to_string(),
                ),
                e => rpc_error_with_code(DebankErrorCode::EvmFailed as i32, e.to_string()),
            },
        }
    }

    pub fn haltreason_to_debank_code(
        #[cfg(not(feature = "optimism"))] reason: &HaltReason,
        #[cfg(feature = "optimism")] reason: &op_revm::result::OpHaltReason,
    ) -> DebankErrorCode {
        match reason {
            #[cfg(not(feature = "optimism"))]
            HaltReason::OutOfFunds => DebankErrorCode::BalanceExhausted,
            #[cfg(not(feature = "optimism"))]
            HaltReason::NonceOverflow => DebankErrorCode::NonceError,
            #[cfg(not(feature = "optimism"))]
            HaltReason::OutOfGas(_) => DebankErrorCode::GasExhausted,
            #[cfg(feature = "optimism")]
            op_revm::result::OpHaltReason::Base(HaltReason::OutOfFunds) => {
                DebankErrorCode::BalanceExhausted
            }
            #[cfg(feature = "optimism")]
            op_revm::result::OpHaltReason::Base(HaltReason::NonceOverflow) => {
                DebankErrorCode::NonceError
            }
            #[cfg(feature = "optimism")]
            op_revm::result::OpHaltReason::Base(HaltReason::OutOfGas(_)) => {
                DebankErrorCode::GasExhausted
            }
            _ => DebankErrorCode::EvmFailed,
        }
    }

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

        let state;

        if block_ctx.block_type == BlockType::Equals {
            state = self.db.state_at(block_ctx.block_id).map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
        } else {
            state = self
                .db
                .state_at(BlockId::Number(BlockNumberOrTag::Latest))
                .map_err(|e| {
                    rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
                })?;
        }
        if state.is_none() {
            return Err(rpc_error_with_code(
                DebankErrorCode::BlockNotFound as i32,
                "block not found".to_string(),
            ));
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
        Ok(block.into())
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
        Ok(block.into())
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
        Ok(block.into())
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

    fn debank_get_storage_at_impl(
        &self,
        address: Address,
        index: H256,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<H256> {
        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let state = EvmStorageWrapper(state);
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
            if account.code_hash.is_zero() || account.code_hash == KECCAK256_EMPTY {
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
        state_override: Option<StateOverride>,
        fast_fail: Option<bool>,
        _use_parallel: Option<bool>,
        _disable_cache: Option<bool>,
    ) -> RpcResult<DebankMultiCallResp> {
        let cfg = self.cfg.clone();

        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let block = state.block_info_arc().map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;

        let (tx, rx) = oneshot::channel();

        tokio::task::spawn_blocking(move || {
            let rsp = Self::debank_multi_call_from_state(
                requests,
                cfg,
                state,
                block,
                block_overrides,
                state_override,
                fast_fail.unwrap_or_default(),
            );
            if let Err(e) = tx.send(rsp) {
                error!("Failed to send multi_call result: {:?}", e);
            }
        });

        let rsp = timeout(self.time_out, rx)
            .await
            .map_err(|_| {
                rpc_error_with_code(
                    DebankErrorCode::TimeOut as i32,
                    "MultiCall timed out".to_string(),
                )
            })? // 超时错误
            .map_err(|_| internal_rpc_err("MultiCall failed".to_string()))?; // 发送失败错误

        rsp
    }

    fn debank_multi_call_from_state(
        requests: Vec<CallRequest>,
        cfg: CfgEnv<SpecId>,
        state: DB::StateDB,
        block: Arc<Block<Transaction>>,
        block_overrides: Option<BlockOverrides>,
        state_override: Option<StateOverride>,
        fast_fail: bool,
    ) -> RpcResult<DebankMultiCallResp> {
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
            let mut block_env = block_env_from_block(&block);
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
            let cfg = cfg.clone();
            let mut db = CacheDB::new(EvmStorageWrapper(state.clone()));
            let tx = create_txn_env(&block_env, request, &db, &cfg)?;
            if let Some(overrides) = block_overrides.clone() {
                super::utils::apply_block_overrides(overrides, &mut db, &mut block_env);
            }
            if let Some(state_override) = state_override.clone() {
                super::utils::apply_state_overrides(state_override, &mut db)?;
            }
            let mut evm =
                utils::create_evm_from_state(block_env.clone(), cfg.clone(), db, NoOpInspector {});

            let res = evm.transact(tx).map_err(|e| Self::evm_to_debank_error(e))?;
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
                    code: Self::haltreason_to_debank_code(&reason) as i32,
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
                let res = Self::get_balance_from_state(EvmStorageWrapper(state), user_addr)
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

    async fn debank_simulate_transactions_impl(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<DebankSimulateResp> {
        let cfg = self.cfg.clone();
        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let block = state.block_info_arc().map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;

        let (tx, rx) = oneshot::channel();

        tokio::task::spawn_blocking(move || {
            let rsp =
                Self::debank_call_many_and_trace(requests, cfg, state, block, block_overrides);
            if let Err(e) = tx.send(rsp) {
                error!("Failed to send multi_call result: {:?}", e);
            }
        });
        let rsp = rx.await.map_err(|_| {
            rpc_error_with_code(
                DebankErrorCode::InternalError as i32,
                "receive simulate rsp failed".to_string(),
            )
        })?;
        rsp
    }

    fn debank_call_many_and_trace(
        txs: Vec<CallRequest>,
        cfg: CfgEnv<SpecId>,
        state: DB::StateDB,
        block: Arc<Block<Transaction>>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<DebankSimulateResp> {
        let mut block_env = block_env_from_block(&block);
        let mut stats = DebankSimulateStats {
            block_num: block.header.number,
            block_time: block.header.timestamp,
            block_hash: block.header.hash,
            success: true,
        };
        let mut memory_db = CacheDB::new(EvmStorageWrapper(state));
        if let Some(overrides) = block_overrides.clone() {
            super::utils::apply_block_overrides(overrides, &mut memory_db, &mut block_env);
        }
        let mut tx_index: u64 = 0;
        let mut results: Vec<DebankSingleSimulateResult> = Vec::new();
        for tx in txs {
            let tx_info = TransactionInfo {
                hash: Some(H256::random()),
                index: Some(tx_index),
                block_hash: Some(block.header.hash),
                block_number: Some(block.header.number),
                base_fee: block.header.base_fee_per_gas,
            };
            tx_index += 1;
            if let Some(last_res) = results.last() {
                if last_res.code != 0 {
                    results.push(last_res.clone());
                    continue;
                }
            }
            let tx = create_txn_env(&block_env, tx, &memory_db, &cfg)?;
            let trace_cfg = TracingInspectorConfig::default_parity().set_record_logs(true);
            let mut inspector = TracingInspector::new(trace_cfg);
            let mut evm = utils::create_evm_from_state(
                block_env.clone(),
                cfg.clone(),
                &mut memory_db,
                &mut inspector,
            );

            let exec_res = evm
                .transact_commit(tx)
                .map_err(|e| Self::evm_to_debank_error(e))?;
            drop(evm);
            match exec_res {
                ExecutionResult::Revert { gas_used, output } => {
                    let reason =
                        decode_revert_reason(&output).unwrap_or("Reason Unknown".to_string());
                    let pre_res = DebankSingleSimulateResult {
                        code: DebankErrorCode::EvmRevert as i32,
                        err: reason,
                        gas_used,
                        ..Default::default()
                    };
                    stats.success = false;
                    results.push(pre_res);
                }
                ExecutionResult::Halt { reason, gas_used } => {
                    let code = Self::haltreason_to_debank_code(&reason);
                    let pre_res = DebankSingleSimulateResult {
                        code: code as i32,
                        err: format!("Halted: {:?}", reason),
                        gas_used,
                        ..Default::default()
                    };
                    stats.success = false;
                    results.push(pre_res);
                }
                ExecutionResult::Success { gas_used, .. } => {
                    let call_traces = inspector.into_traces();

                    let (traces, events) = build_debank_traces(tx_info.hash.unwrap(), call_traces);

                    let pre_res = DebankSingleSimulateResult {
                        gas_used,
                        traces,
                        events,
                        ..Default::default()
                    };
                    results.push(pre_res);
                }
            }
        }
        Ok(DebankSimulateResp { stats, results })
    }

    async fn debank_estimate_gas_impl(
        &self,
        request: CallRequest,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<U256> {
        let cfg = self.cfg.clone();
        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let block = state.block_info_arc().map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;

        let (tx, rx) = oneshot::channel();

        tokio::task::spawn_blocking(move || {
            let rsp = Self::debank_estimate_gas_many(request, cfg, state, block, block_overrides);
            if let Err(e) = tx.send(rsp) {
                error!("Failed to send multi_call result: {:?}", e);
            }
        });

        let rsp = rx
            .await
            .map_err(|_| internal_rpc_err("MultiCall failed".to_string()))?;
        rsp
    }

    fn debank_estimate_gas_many(
        mut request: CallRequest,
        cfg: CfgEnv<SpecId>,
        state: DB::StateDB,
        block: Arc<Block<Transaction>>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<U256> {
        // set nonce to None so that the correct nonce is chosen by the EVM
        request.nonce = None;
        let mut block_env = block_env_from_block(&block);
        // Keep a copy of gas related request values
        let tx_request_gas_limit = request.gas;
        // the gas limit of the corresponding block
        let block_env_gas_limit = block.header.gas_limit;
        let mut highest_gas_limit = tx_request_gas_limit
            .map(|tx_gas_limit| {
                if tx_gas_limit > block_env_gas_limit {
                    tx_gas_limit
                } else {
                    block_env_gas_limit
                }
            })
            .unwrap_or(block_env_gas_limit);
        let mut memory_db = CacheDB::new(EvmStorageWrapper(state));
        if let Some(overrides) = block_overrides.clone() {
            utils::apply_block_overrides(overrides, &mut memory_db, &mut block_env);
        }
        let mut tx = create_txn_env(&block_env, request, &memory_db, &cfg)?;
        if tx.input().is_empty() {
            if let TransactTo::Call(to) = tx.kind() {
                if let Ok(account) = memory_db.basic_ref(to) {
                    let no_code_callee = account
                        .map(|account| {
                            account.is_empty_code_hash() || account.code_hash().is_zero()
                        })
                        .unwrap_or(true);
                    if no_code_callee {
                        let mut tx = tx.clone();
                        #[cfg(not(feature = "optimism"))]
                        {
                            tx.gas_limit = crate::api_impl::debank::MIN_TRANSACTION_GAS;
                        }
                        #[cfg(feature = "optimism")]
                        {
                            tx.base.gas_limit = MIN_TRANSACTION_GAS;
                        }
                        let mut evm = utils::create_evm_from_state(
                            block_env.clone(),
                            cfg.clone(),
                            memory_db.clone(),
                            NoOpInspector {},
                        );

                        let exec_res =
                            evm.transact(tx).map_err(|e| Self::evm_to_debank_error(e))?;
                        if exec_res.result.is_success() {
                            return Ok(U256::from(MIN_TRANSACTION_GAS));
                        }
                    }
                }
            }
        }
        if tx.gas_price() > 0 {
            let caller = memory_db.basic_ref(tx.caller()).map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
            let balance = caller
                .map(|acc| acc.balance)
                .unwrap_or_default()
                .checked_sub(tx.value())
                .ok_or_else(|| {
                    rpc_error_with_code(
                        DebankErrorCode::BalanceExhausted as i32,
                        "Insufficient funds".to_string(),
                    )
                })?;
            let gas_limit: u64 = balance
                .checked_div(U256::from(tx.gas_price()))
                .unwrap_or_default()
                .try_into()
                .unwrap();
            highest_gas_limit = highest_gas_limit.min(gas_limit);
        }

        #[cfg(not(feature = "optimism"))]
        {
            tx.gas_limit = tx.gas_limit().min(highest_gas_limit);
        }
        #[cfg(feature = "optimism")]
        {
            tx.base.gas_limit = tx.gas_limit().min(highest_gas_limit);
        }
        let mut evm = utils::create_evm_from_state(
            block_env.clone(),
            cfg.clone(),
            memory_db.clone(),
            NoOpInspector {},
        );

        let res = evm
            .transact(tx.clone())
            .map_err(|e| Self::evm_to_debank_error(e))?;

        let gas_refund = match res.result {
            ExecutionResult::Success { gas_refunded, .. } => gas_refunded,
            ExecutionResult::Halt { reason, .. } => {
                let code = Self::haltreason_to_debank_code(&reason);
                return Err(rpc_error_with_code(
                    code as i32,
                    format!("Halted: {:?}", reason),
                ));
            }
            ExecutionResult::Revert { output, .. } => {
                let reason = decode_revert_reason(&output).unwrap_or("Reason Unknown".to_string());
                return Err(rpc_error_with_code(
                    DebankErrorCode::EvmRevert as i32,
                    reason,
                ));
            }
        };

        highest_gas_limit = tx.gas_limit();
        let mut gas_used = res.result.gas_used();
        let mut lowest_gas_limit = gas_used.saturating_sub(1);

        let optimistic_gas_limit = (gas_used + gas_refund + CALL_STIPEND_GAS) * 64 / 63;

        fn update_estimated_gas_range(
            #[cfg(not(feature = "optimism"))] result: &ExecutionResult<HaltReason>,
            #[cfg(feature = "optimism")] result: &ExecutionResult<op_revm::result::OpHaltReason>,
            tx_gas_limit: u64,
            highest_gas_limit: &mut u64,
            lowest_gas_limit: &mut u64,
        ) -> RpcResult<()> {
            match result {
                ExecutionResult::Success { .. } => {
                    // Cap the highest gas limit with the succeeding gas limit.
                    *highest_gas_limit = tx_gas_limit;
                }
                ExecutionResult::Revert { .. } => {
                    // Increase the lowest gas limit.
                    *lowest_gas_limit = tx_gas_limit;
                }
                ExecutionResult::Halt { reason, .. } => match reason {
                    #[cfg(not(feature = "optimism"))]
                    HaltReason::OutOfGas(_) | HaltReason::InvalidFEOpcode => {
                        *lowest_gas_limit = tx_gas_limit;
                    }
                    #[cfg(feature = "optimism")]
                    op_revm::result::OpHaltReason::Base(HaltReason::OutOfGas(_))
                    | op_revm::result::OpHaltReason::Base(HaltReason::InvalidFEOpcode) => {
                        *lowest_gas_limit = tx_gas_limit;
                    }
                    err => {
                        return Err(rpc_error_with_code(
                            DebankErrorCode::InternalError as i32,
                            format!("Halted: {:?}", err),
                        ))
                    }
                },
            };

            Ok(())
        }

        if optimistic_gas_limit < highest_gas_limit {
            #[cfg(not(feature = "optimism"))]
            {
                tx.gas_limit = optimistic_gas_limit;
            }
            #[cfg(feature = "optimism")]
            {
                tx.base.gas_limit = optimistic_gas_limit;
            }
            let mut evm = utils::create_evm_from_state(
                block_env.clone(),
                cfg.clone(),
                memory_db.clone(),
                NoOpInspector {},
            );

            let res = evm
                .transact(tx.clone())
                .map_err(|e| Self::evm_to_debank_error(e))?;
            gas_used = res.result.gas_used();
            update_estimated_gas_range(
                &res.result,
                optimistic_gas_limit,
                &mut highest_gas_limit,
                &mut lowest_gas_limit,
            )?;
        };

        // Pick a point that's close to the estimated gas
        let mut mid_gas_limit = std::cmp::min(
            gas_used * 3,
            ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64,
        );

        while (highest_gas_limit - lowest_gas_limit) > 1 {
            if (highest_gas_limit - lowest_gas_limit) as f64 / (highest_gas_limit as f64)
                < ESTIMATE_GAS_ERROR_RATIO
            {
                break;
            };

            #[cfg(not(feature = "optimism"))]
            {
                tx.gas_limit = mid_gas_limit;
            }
            #[cfg(feature = "optimism")]
            {
                tx.base.gas_limit = mid_gas_limit;
            }

            let mut evm = utils::create_evm_from_state(
                block_env.clone(),
                cfg.clone(),
                memory_db.clone(),
                NoOpInspector {},
            );
            let res = evm.transact(tx.clone());

            match res {
                Err(EVMError::Transaction(invaild_tx_err)) => match invaild_tx_err {
                    #[cfg(not(feature = "optimism"))]
                    InvalidTransaction::CallerGasLimitMoreThanBlock => {
                        highest_gas_limit = mid_gas_limit;
                    }
                    #[cfg(not(feature = "optimism"))]
                    InvalidTransaction::CallGasCostMoreThanGasLimit {
                        initial_gas: _initial_gas,
                        gas_limit: _gas_limit,
                    } => {
                        lowest_gas_limit = mid_gas_limit;
                    }
                    #[cfg(feature = "optimism")]
                    OpTransactionError::Base(InvalidTransaction::CallerGasLimitMoreThanBlock) => {
                        highest_gas_limit = mid_gas_limit;
                    }
                    #[cfg(feature = "optimism")]
                    OpTransactionError::Base(InvalidTransaction::CallGasCostMoreThanGasLimit {
                        initial_gas: _initial_gas,
                        gas_limit: _gas_limit,
                    }) => {
                        lowest_gas_limit = mid_gas_limit;
                    }
                    e => {
                        return Err(rpc_error_with_code(
                            DebankErrorCode::EvmFailed as i32,
                            format!("Invalid transaction: {:?}", e),
                        ))
                    }
                },
                Err(EVMError::Database(e)) => {
                    return Err(rpc_error_with_code(
                        DebankErrorCode::DataBaseFailed as i32,
                        e.to_string(),
                    ))
                }
                Err(EVMError::Header(e)) => {
                    return Err(rpc_error_with_code(
                        DebankErrorCode::InvalidParams as i32,
                        e.to_string(),
                    ))
                }
                Err(e) => {
                    return Err(rpc_error_with_code(
                        DebankErrorCode::InternalError as i32,
                        e.to_string(),
                    ))
                }
                Ok(res) => {
                    update_estimated_gas_range(
                        &res.result,
                        mid_gas_limit,
                        &mut highest_gas_limit,
                        &mut lowest_gas_limit,
                    )?;
                }
            };

            mid_gas_limit = ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64;
        }
        Ok(U256::from(highest_gas_limit))
    }
}

#[async_trait::async_trait]
impl<DB: EvmStorageRead + BlockIndex + Send + Sync + 'static> DebankApiServer for ApiImpl<DB> {
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

    async fn get_storage_at(
        &self,
        address: Address,
        position: JsonStorageKey,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<H256> {
        self.debank_get_storage_at_impl(address, position.as_b256(), block_ctx)
    }

    async fn contract_multi_call(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
        state_override: Option<StateOverride>,
        fast_fail: Option<bool>,
        use_parallel: Option<bool>,
        disable_cache: Option<bool>,
    ) -> RpcResult<DebankMultiCallResp> {
        self.contract_multi_call_impl(
            requests,
            block_ctx,
            block_overrides,
            state_override,
            fast_fail,
            use_parallel,
            disable_cache,
        )
        .await
    }

    async fn simulate_transactions(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<DebankSimulateResp> {
        self.debank_simulate_transactions_impl(requests, block_ctx, block_overrides)
            .await
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

    async fn estimate_gas(
        &self,
        request: CallRequest,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<U256> {
        self.debank_estimate_gas_impl(request, block_ctx, block_overrides)
            .await
    }
}
