use super::utils;
use crate::api::{DebankApiClient, DebankApiServer};
use crate::api_impl::core::{
    Api, ApiCore, EvmExecutor, GetHaltReason, GetTransactionError, ToJsonRpcError, TxSetter,
};
use crate::api_impl::utils::build_debank_traces;
use crate::error::{internal_rpc_err, rpc_error_with_code};

use alloy::rpc::types::state::StateOverride;
use alloy::sol_types::{decode_revert_reason, SolValue};
use jsonrpsee::{core::RpcResult, http_client::HttpClient};
use leafage_evm_storage::{BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{
    block_env_from_block, Address, BlockId, BlockNumberOrTag, BlockOverrides, BlockType, Bytes,
    CallRequest, DebankBlock, DebankBlockContext, DebankErrorCode, DebankMultiCallResp,
    DebankMultiCallStats, DebankSimulateResp, DebankSimulateStats, DebankSingleCallResult,
    DebankSingleSimulateResult, Header, JsonStorageKey, TransactionInfo, H256, KECCAK256_EMPTY,
    U256,
};
use revm::bytecode::OpCode;
use revm::context::result::InvalidTransaction;
use revm::context::result::{ExecutionResult, HaltReason};
use revm::context::{TransactTo, Transaction as TransactionTrait};
use revm::database::{CacheDB, DatabaseRef};
use revm_inspectors::tracing::{OpcodeFilter, TracingInspectorConfig};
use std::str::FromStr;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::error;

pub const MIN_TRANSACTION_GAS: u64 = 21_000u64;

pub const CALL_STIPEND_GAS: u64 = 2_300;

pub const ESTIMATE_GAS_ERROR_RATIO: f64 = 0.015;

impl<C> Api<C> {
    pub fn new(core: C) -> Self {
        Self {
            inner: Arc::new(core),
        }
    }

    pub fn get_balance_from_state<StateDB>(state: StateDB, address: Address) -> RpcResult<U256>
    where
        StateDB: DatabaseRef,
    {
        let account = state
            .basic_ref(address.0.into())
            .map_err(|e| internal_rpc_err(e.to_string()))?;
        let balance = account.map(|a| a.balance);
        Ok(balance.unwrap_or_default().into())
    }
}

impl<C> Api<C>
where
    C: ApiCore,
    C::DB: EvmStorageRead + BlockIndex,
    C::TransactionError: ToJsonRpcError + GetTransactionError,
    C::EvmHaltReason: std::fmt::Debug + Clone + GetHaltReason,
    DebankErrorCode: From<<C as EvmExecutor>::EvmHaltReason>,
{
    fn should_try_historical(&self, block_ctx: &Option<DebankBlockContext>) -> Option<&HttpClient> {
        let client = self.inner.historical_client()?;

        if let Some(ctx) = block_ctx {
            match &ctx.block_id {
                BlockId::Hash(_) => Some(client),
                BlockId::Number(BlockNumberOrTag::Number(num)) => {
                    if self.inner.historical_height().map_or(false, |h| *num < h) {
                        Some(client)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        } else {
            None
        }
    }

    fn debank_version(&self) -> RpcResult<String> {
        Ok(self.inner.evm_cfg().version.clone())
    }

    fn debank_get_state_by_ctx_impl(
        &self,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<<C::DB as EvmStorageRead>::StateDB> {
        if block_ctx.is_none() {
            let state = self
                .inner
                .db()
                .state_at(BlockId::Number(BlockNumberOrTag::Latest))
                .map_err(|e| {
                    rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
                })?;
            return Ok(state.unwrap());
        }

        let block_ctx = block_ctx.unwrap();

        let state;

        if block_ctx.block_type == BlockType::Equals {
            state = self.inner.db().state_at(block_ctx.block_id).map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
        } else {
            state = self
                .inner
                .db()
                .state_at(BlockId::Number(BlockNumberOrTag::Latest))
                .map_err(|e| {
                    rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
                })?;
        }
        if state.is_none() {
            if self.inner.evm_cfg().is_archive {
                return Err(rpc_error_with_code(
                    DebankErrorCode::InvalidBlockID as i32,
                    format!("block {:?} is invalid", block_ctx.block_id),
                ));
            } else {
                return Err(rpc_error_with_code(
                    DebankErrorCode::BlockNotFound as i32,
                    format!("block {:?} not found for state node", block_ctx.block_id),
                ));
            }
        }
        let state = state.unwrap();
        Ok(state)
    }

    fn debank_get_latest_block_impl(&self) -> RpcResult<DebankBlock> {
        let block = self
            .inner
            .db()
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
            .inner
            .db()
            .get_block_by_id_arc(BlockId::Number(BlockNumberOrTag::Number(number)))
            .map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
        if block.is_none() {
            if self.inner.evm_cfg().is_archive {
                return Err(rpc_error_with_code(
                    DebankErrorCode::InvalidBlockID as i32,
                    format!("block height {:?} is invalid", height),
                ));
            } else {
                return Err(rpc_error_with_code(
                    DebankErrorCode::BlockNotFound as i32,
                    format!("block height {:?} not found for state node", height),
                ));
            }
        }

        let block = block.unwrap();
        Ok(block.into())
    }

    fn debank_get_block_by_id_impl(&self, id: H256) -> RpcResult<DebankBlock> {
        let block = self
            .inner
            .db()
            .get_block_by_id_arc(BlockId::Hash(id.into()))
            .map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
        if block.is_none() {
            if self.inner.evm_cfg().is_archive {
                return Err(rpc_error_with_code(
                    DebankErrorCode::InvalidBlockID as i32,
                    format!("block id {:?} is invalid", id),
                ));
            } else {
                return Err(rpc_error_with_code(
                    DebankErrorCode::BlockNotFound as i32,
                    format!("block id {:?} not found", id),
                ));
            }
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
        let state = EvmStorageWrapper {
            db: state,
            ovm_address: self.inner.evm_cfg().ovm_address.clone(),
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        };
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
        if let Some(vb) = self.inner.virtual_balance() {
            return Ok(vb);
        }
        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let state = EvmStorageWrapper {
            db: state,
            ovm_address: self.inner.evm_cfg().ovm_address.clone(),
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        };
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
        let state = EvmStorageWrapper {
            db: state,
            ovm_address: self.inner.evm_cfg().ovm_address.clone(),
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
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

    fn debank_get_code_impl(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<Bytes> {
        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let state = EvmStorageWrapper {
            db: state,
            ovm_address: self.inner.evm_cfg().ovm_address.clone(),
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        };
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
            Ok(code.original_bytes().0.clone().into())
        }
    }

    fn debank_eth_erc20_handle<StateDB>(
        block_header: &Header,
        state: StateDB,
        request: CallRequest,
        ovm_address: Option<H256>,
        normalize_state_key: bool,
    ) -> DebankSingleCallResult
    where
        StateDB: leafage_evm_storage::StateDB,
    {
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
                let res = Self::get_balance_from_state(
                    EvmStorageWrapper {
                        db: state,
                        ovm_address,
                        normalize_state_key,
                    },
                    user_addr,
                )
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
            } else if data[0..4] == [0x6c, 0x4b, 0x6e, 0x28] {
                let block_num = U256::from(block_header.number);
                let block_hash = block_header.hash;
                return DebankSingleCallResult {
                    code: 0,
                    err: "".to_string(),
                    from_cache: false,
                    result: Bytes::from((block_num, block_hash).abi_encode()),
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

    fn debank_single_call_from_state_impl_inner(
        &self,
        state: &<C::DB as EvmStorageRead>::StateDB,
        request: CallRequest,
        block_overrides: Option<BlockOverrides>,
        state_override: Option<StateOverride>,
    ) -> RpcResult<DebankSingleCallResult> {
        let block = state.block_info_arc().map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;
        let mut block_env = block_env_from_block(&block);
        let start = std::time::Instant::now();

        // Collect ERC20 token address if token_collector is enabled
        if let Some(collector) = self.inner.token_collector() {
            let to = request.to.and_then(|txkind| txkind.to().copied());
            let data = request.input.input().map(|d| d.as_ref()).unwrap_or(&[]);
            collector.maybe_collect_call(to, data);
        }

        if let Some(txkind) = request.to {
            if let Some(address) = txkind.to() {
                if *address
                    == Address::from_str("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee").unwrap()
                {
                    let mut res = Self::debank_eth_erc20_handle(
                        &block.header,
                        state.clone(),
                        request,
                        self.inner.evm_cfg().ovm_address.clone(),
                        self.inner.evm_cfg().normalize_state_key,
                    );
                    res.time_cost = start.elapsed().as_secs_f64();
                    return Ok(res);
                }
            }
        }
        let mut db = CacheDB::new(EvmStorageWrapper {
            db: state.clone(),
            ovm_address: self.inner.evm_cfg().ovm_address.clone(),
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        });
        if let Some(overrides) = block_overrides.clone() {
            super::utils::apply_block_overrides(
                overrides,
                &mut db,
                &mut block_env,
                block.header.clone(),
            );
        }
        if let Some(state_override) = state_override.clone() {
            super::utils::apply_state_overrides(state_override, &mut db)?;
        }
        let tx = self.inner.create_txn_env(
            &block_env,
            request,
            &db,
            self.inner.evm_cfg().cfg.chain_id,
        )?;
        let mut res: DebankSingleCallResult = self
            .inner
            .transact(&block_env, &db, tx)
            .map_err(|e| e.to_rpc_error())?
            .into();
        res.time_cost = start.elapsed().as_secs_f64();
        Ok(res)
    }

    fn debank_multi_call_from_state_impl_inner(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
        state_override: Option<StateOverride>,
        fast_fail: bool,
        cancel_token: CancellationToken,
    ) -> RpcResult<DebankMultiCallResp> {
        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let block = state.block_info_arc().map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;
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
            if cancel_token.is_cancelled() {
                return Err(internal_rpc_err(
                    "multicall cancelled by caller".to_string(),
                ));
            }
            if fast_fail && !results.is_empty() && results.last().unwrap().code != 0 {
                let res = results.last().unwrap().clone();
                results.push(res);
                continue;
            }
            let res = self.debank_single_call_from_state_impl_inner(
                &state,
                request,
                block_overrides.clone(),
                state_override.clone(),
            )?;
            if res.code != 0 {
                stats.success = false;
            }
            results.push(res);
        }
        Ok(DebankMultiCallResp { stats, results })
    }

    pub async fn contract_multi_call_impl(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
        state_override: Option<StateOverride>,
        fast_fail: Option<bool>,
        _use_parallel: Option<bool>,
        _disable_cache: Option<bool>,
    ) -> RpcResult<DebankMultiCallResp> {
        let this = self.clone();
        utils::spawn_blocking_with_cancel(move |token| {
            this.debank_multi_call_from_state_impl_inner(
                requests,
                block_ctx,
                block_overrides,
                state_override,
                fast_fail.unwrap_or_default(),
                token,
            )
        })
        .await
        .inspect_err(|err| error!("Failed to spawn contract_multi_call result: {:?}", err))
        .map_err(|_| internal_rpc_err("multi call failed"))?
    }

    async fn debank_simulate_transactions_impl(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<DebankSimulateResp> {
        let this = self.clone();
        utils::spawn_blocking_with_cancel(move |token| {
            this.debank_simulate_transactions_impl_inner(
                requests,
                block_ctx,
                block_overrides,
                token,
            )
        })
        .await
        .inspect_err(|err| error!("Failed to spawn simulate_transactions result: {:?}", err))
        .map_err(|_| internal_rpc_err("simulate transactions failed"))?
    }

    fn debank_simulate_transactions_impl_inner(
        &self,
        txs: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
        cancel_token: CancellationToken,
    ) -> RpcResult<DebankSimulateResp> {
        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let block = state.block_info_arc().map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;
        let mut block_env = block_env_from_block(&block);
        let mut stats = DebankSimulateStats {
            block_num: block.header.number,
            block_time: block.header.timestamp,
            block_hash: block.header.hash,
            success: true,
        };
        let mut memory_db = CacheDB::new(EvmStorageWrapper {
            db: state,
            ovm_address: self.inner.evm_cfg().ovm_address.clone(),
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        });
        if let Some(overrides) = block_overrides {
            let header = super::utils::apply_block_overrides(
                overrides,
                &mut memory_db,
                &mut block_env,
                block.header.clone(),
            );
            if let Some(header) = header {
                self.inner
                    .apply_pre_execution_changes(header, &block_env, &mut memory_db)?;
            }
        }
        let mut tx_index: u64 = 0;
        let mut results: Vec<DebankSingleSimulateResult> = Vec::new();
        for tx in txs {
            if cancel_token.is_cancelled() {
                return Err(internal_rpc_err(
                    "simulate transactions cancelled by caller".to_string(),
                ));
            }
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
            let mut trace_cfg = TracingInspectorConfig::default_parity()
                .set_record_logs(true)
                .set_steps(true);
            trace_cfg.record_opcodes_filter = Some(OpcodeFilter::new().enabled(OpCode::SSTORE));
            let tx = self.inner.create_txn_env(
                &block_env,
                tx,
                &memory_db,
                self.inner.evm_cfg().cfg.chain_id,
            )?;
            let (exec_res, (traces, events)) = self
                .inner
                .inspect_tx_commit(
                    &block_env,
                    &mut memory_db,
                    trace_cfg,
                    |inspector| build_debank_traces(tx_info.hash.unwrap(), inspector.into_traces()),
                    tx,
                )
                .map_err(|e| e.to_rpc_error())?;
            let mut pre_res: DebankSingleSimulateResult = exec_res.into();
            pre_res.traces = traces;
            pre_res.events = events;
            if pre_res.code != 0 {
                stats.success = false;
            }
            results.push(pre_res);
        }
        Ok(DebankSimulateResp { stats, results })
    }

    fn debank_estimate_gas_inner(
        &self,
        mut request: CallRequest,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
        cancel_token: CancellationToken,
    ) -> RpcResult<U256> {
        let state = self.debank_get_state_by_ctx_impl(block_ctx)?;
        let block = state.block_info_arc().map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;
        // set nonce to None so that the correct nonce is chosen by the EVM
        request.nonce = None;
        let mut block_env = block_env_from_block(&block);
        let mut memory_db = CacheDB::new(EvmStorageWrapper {
            db: state,
            ovm_address: self.inner.evm_cfg().ovm_address.clone(),
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        });
        if let Some(overrides) = block_overrides.clone() {
            utils::apply_block_overrides(
                overrides,
                &mut memory_db,
                &mut block_env,
                block.header.clone(),
            );
        }
        // Keep a copy of gas related request values
        let tx_request_gas_limit = request.gas;
        // the gas limit of the corresponding block
        let block_env_gas_limit = block_env.gas_limit;
        let max_gas_limit = self
            .inner
            .evm_cfg()
            .cfg
            .tx_gas_limit_cap
            .map_or_else(|| block_env_gas_limit, |cap| cap.min(block_env_gas_limit));
        let mut highest_gas_limit = tx_request_gas_limit
            .map(|tx_gas_limit| {
                if tx_gas_limit > max_gas_limit {
                    tx_gas_limit
                } else {
                    max_gas_limit
                }
            })
            .unwrap_or(max_gas_limit);
        let mut tx = self.inner.create_txn_env(
            &block_env,
            request.clone(),
            &memory_db,
            self.inner.evm_cfg().cfg.chain_id,
        )?;
        // Skip no_code_callee early return for Tempo — TIP-1000 nonce==0 surcharge
        // adds 250k gas that this optimization doesn't account for. The early return
        // would incorrectly return MIN_TRANSACTION_GAS (21000) when the actual
        // required gas is 271000+.
        if self.inner.virtual_balance().is_none() && tx.input().is_empty() {
            if let TransactTo::Call(to) = tx.kind() {
                if let Ok(account) = memory_db.basic_ref(to) {
                    let no_code_callee = account
                        .map(|account| {
                            account.is_empty_code_hash() || account.code_hash().is_zero()
                        })
                        .unwrap_or(true);
                    if no_code_callee {
                        let mut tx = tx.clone();
                        tx.set_gas_limit(MIN_TRANSACTION_GAS);
                        if let Ok(exec_res) =
                            self.inner.transact(&block_env, &memory_db, tx.clone())
                        {
                            if exec_res.is_success() {
                                let l1_overhead = self.inner.estimate_l1_overhead(
                                    &block,
                                    &block_env,
                                    tx,
                                    &memory_db,
                                );
                                return Ok(U256::from(
                                    MIN_TRANSACTION_GAS.saturating_add(l1_overhead),
                                ));
                            }
                        }
                    }
                }
            }
        }
        if tx.gas_price() > 0 {
            let gas_limit = self
                .inner
                .gas_allowance(&request, &tx, &memory_db, &block_env)?;
            highest_gas_limit = highest_gas_limit.min(gas_limit);
        }
        tx.set_gas_limit(tx.gas_limit().min(highest_gas_limit));

        let res = self
            .inner
            .transact(&block_env, &memory_db, tx.clone())
            .map_err(|e| e.to_rpc_error())?;

        let gas_refund = match res {
            ExecutionResult::Success { gas, .. } => gas.inner_refunded(),
            ExecutionResult::Halt { reason, .. } => {
                let code = DebankErrorCode::from(reason.clone());
                return Err(rpc_error_with_code(
                    code as i32,
                    format!("Halted: {:?}", reason),
                ));
            }
            ExecutionResult::Revert { output, .. } => {
                let reason =
                    decode_revert_reason(&output).unwrap_or("execution revert".to_string());
                return Err(rpc_error_with_code(
                    DebankErrorCode::EvmRevert as i32,
                    reason,
                ));
            }
        };

        highest_gas_limit = tx.gas_limit();
        let mut gas_used = res.gas_used();
        let mut lowest_gas_limit = gas_used.saturating_sub(1);

        let optimistic_gas_limit = (gas_used + gas_refund + CALL_STIPEND_GAS) * 64 / 63;

        if optimistic_gas_limit < highest_gas_limit {
            tx.set_gas_limit(optimistic_gas_limit);
            let res = self
                .inner
                .transact(&block_env, &memory_db, tx.clone())
                .map_err(|e| e.to_rpc_error())?;
            gas_used = res.gas_used();
            update_estimated_gas_range(
                &res,
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
            if cancel_token.is_cancelled() {
                return Err(internal_rpc_err(
                    "estimate gas cancelled by caller".to_string(),
                ));
            }
            if (highest_gas_limit - lowest_gas_limit) as f64 / (highest_gas_limit as f64)
                < ESTIMATE_GAS_ERROR_RATIO
            {
                break;
            };

            tx.set_gas_limit(mid_gas_limit);

            let res = self.inner.transact(&block_env, &memory_db, tx.clone());

            match res {
                Err(e) => {
                    if let Some(invalid_tx_err) = e.get_transaction_error() {
                        match invalid_tx_err {
                            InvalidTransaction::CallerGasLimitMoreThanBlock
                            | InvalidTransaction::TxGasLimitGreaterThanCap { .. } => {
                                highest_gas_limit = mid_gas_limit;
                            }
                            InvalidTransaction::CallGasCostMoreThanGasLimit { .. }
                            | InvalidTransaction::GasFloorMoreThanGasLimit { .. } => {
                                lowest_gas_limit = mid_gas_limit;
                            }
                            e => {
                                return Err(rpc_error_with_code(
                                    DebankErrorCode::EvmFailed as i32,
                                    format!("Invalid transaction: {:?}", e),
                                ))
                            }
                        }
                    } else {
                        return Err(e.to_rpc_error());
                    }
                }
                Ok(res) => {
                    update_estimated_gas_range(
                        &res,
                        mid_gas_limit,
                        &mut highest_gas_limit,
                        &mut lowest_gas_limit,
                    )?;
                }
            };

            mid_gas_limit = ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64;
        }

        let buffer = self.inner.evm_cfg().estimate_gas_buffer;
        let final_gas = if buffer > 100 {
            let buffered = (highest_gas_limit as u128 * buffer as u128) / 100;
            buffered.min(u64::MAX as u128) as u64
        } else {
            highest_gas_limit
        };

        tx.set_gas_limit(final_gas);
        let l1_overhead = self.inner.estimate_l1_overhead(&block, &block_env, tx.clone(), &memory_db);

        Ok(U256::from(final_gas.saturating_add(l1_overhead)))
    }

    async fn debank_estimate_gas_impl(
        &self,
        request: CallRequest,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<U256> {
        let this = self.clone();
        utils::spawn_blocking_with_cancel(move |token| {
            this.debank_estimate_gas_inner(request, block_ctx, block_overrides, token)
        })
        .await
        .inspect_err(|err| error!("Failed to spawn debank_estimate result: {:?}", err))
        .map_err(|_| internal_rpc_err("estimate failed".to_string()))?
    }

    fn block_is_valid_impl(&self, id: H256) -> RpcResult<bool> {
        let block = self
            .inner
            .db()
            .get_block_by_id_arc(BlockId::Hash(id.into()))
            .map_err(|e| {
                rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
            })?;
        if block.is_none() {
            if self.inner.evm_cfg().is_archive {
                return Ok(false);
            } else {
                return Err(rpc_error_with_code(
                    DebankErrorCode::BlockNotFound as i32,
                    "block not found".to_string(),
                ));
            }
        }

        let block = block.unwrap();
        let canonical_block = self
            .inner
            .db()
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

#[inline]
fn update_estimated_gas_range<R: GetHaltReason + Clone>(
    result: &ExecutionResult<R>,
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
        ExecutionResult::Halt { reason, .. } => {
            let reason = reason.get_halt_reason();
            match reason {
                Some(HaltReason::OutOfGas(_)) | Some(HaltReason::InvalidFEOpcode) => {
                    *lowest_gas_limit = tx_gas_limit;
                }
                Some(err) => {
                    return Err(rpc_error_with_code(
                        DebankErrorCode::InternalError as i32,
                        format!("Halted: {:?}", err),
                    ))
                }
                None => {
                    return Err(rpc_error_with_code(
                        DebankErrorCode::InternalError as i32,
                        format!("Halted: UnKnown"),
                    ))
                }
            }
        }
    };

    Ok(())
}

#[inline]
fn combine_errors(
    local_err: jsonrpsee::types::ErrorObjectOwned,
    historical_err: jsonrpsee::core::ClientError,
) -> jsonrpsee::types::ErrorObjectOwned {
    rpc_error_with_code(
        local_err.code(),
        format!(
            "Local error: {}; Historical RPC error: {}",
            local_err.message(),
            historical_err
        ),
    )
}

#[async_trait::async_trait]
impl<C> DebankApiServer for Api<C>
where
    C: ApiCore,
    C::DB: EvmStorageRead + BlockIndex,
    C::TransactionError: ToJsonRpcError + GetTransactionError,
    C::EvmHaltReason: std::fmt::Debug + Clone + GetHaltReason,
    DebankErrorCode: From<<C as EvmExecutor>::EvmHaltReason>,
{
    async fn version(&self) -> RpcResult<String> {
        self.debank_version()
    }

    async fn get_address_nonce(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<U256> {
        match self.debank_get_address_nonce_impl(address, block_ctx.clone()) {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.should_try_historical(&block_ctx) {
                    match historical_client
                        .get_address_nonce(address, block_ctx)
                        .await
                    {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(combine_errors(err, historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn get_address_balance(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<U256> {
        match self.debank_get_address_balance_impl(address, block_ctx.clone()) {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.should_try_historical(&block_ctx) {
                    match historical_client
                        .get_address_balance(address, block_ctx)
                        .await
                    {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(combine_errors(err, historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn get_address_code(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<Bytes> {
        match self.debank_get_code_impl(address, block_ctx.clone()) {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.should_try_historical(&block_ctx) {
                    match historical_client.get_address_code(address, block_ctx).await {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(combine_errors(err, historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn get_storage_at(
        &self,
        address: Address,
        position: JsonStorageKey,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<H256> {
        match self.debank_get_storage_at_impl(address, position.as_b256(), block_ctx.clone()) {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.should_try_historical(&block_ctx) {
                    match historical_client
                        .get_storage_at(address, position, block_ctx)
                        .await
                    {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(combine_errors(err, historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
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
        match self
            .contract_multi_call_impl(
                requests.clone(),
                block_ctx.clone(),
                block_overrides.clone(),
                state_override.clone(),
                fast_fail,
                use_parallel,
                disable_cache,
            )
            .await
        {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.should_try_historical(&block_ctx) {
                    match historical_client
                        .contract_multi_call(
                            requests,
                            block_ctx,
                            block_overrides,
                            state_override,
                            fast_fail,
                            use_parallel,
                            disable_cache,
                        )
                        .await
                    {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(combine_errors(err, historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn simulate_transactions(
        &self,
        requests: Vec<CallRequest>,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<DebankSimulateResp> {
        match self
            .debank_simulate_transactions_impl(
                requests.clone(),
                block_ctx.clone(),
                block_overrides.clone(),
            )
            .await
        {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.should_try_historical(&block_ctx) {
                    match historical_client
                        .simulate_transactions(requests, block_ctx, block_overrides)
                        .await
                    {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(combine_errors(err, historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn get_latest_block(&self) -> RpcResult<DebankBlock> {
        self.debank_get_latest_block_impl()
    }

    async fn get_block_by_height(&self, height: U256) -> RpcResult<DebankBlock> {
        let block_number: u64 = height.try_into().map_err(|_| {
            rpc_error_with_code(
                DebankErrorCode::InvalidParams as i32,
                "block height out of range".to_string(),
            )
        })?;

        if self.inner.historical_client().is_some()
            && self
                .inner
                .historical_height()
                .map_or(false, |h| block_number < h)
        {
            if let Some(historical_client) = self.inner.historical_client() {
                return historical_client
                    .get_block_by_height(height)
                    .await
                    .map_err(|e| {
                        rpc_error_with_code(
                            DebankErrorCode::InternalError as i32,
                            format!("Historical RPC error: {}", e),
                        )
                    });
            }
        }

        self.debank_get_block_by_height_impl(height)
    }

    async fn get_block_by_id(&self, id: H256) -> RpcResult<DebankBlock> {
        match self.debank_get_block_by_id_impl(id) {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.inner.historical_client() {
                    match historical_client.get_block_by_id(id).await {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(combine_errors(err, historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn block_is_valid(&self, id: H256) -> RpcResult<bool> {
        match self.block_is_valid_impl(id) {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.inner.historical_client() {
                    match historical_client.block_is_valid(id).await {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(combine_errors(err, historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn estimate_gas(
        &self,
        request: CallRequest,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<U256> {
        match self
            .debank_estimate_gas_impl(request.clone(), block_ctx.clone(), block_overrides.clone())
            .await
        {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.should_try_historical(&block_ctx) {
                    match historical_client
                        .estimate_gas(request, block_ctx, block_overrides)
                        .await
                    {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(combine_errors(err, historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }
}
