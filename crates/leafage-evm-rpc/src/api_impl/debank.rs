use super::utils;
use crate::api::{DebankApiClient, DebankApiServer};
use crate::api_impl::core::{
    Api, ApiCore, EvmExecutor, GetHaltReason, GetTransactionError, ToJsonRpcError, TxSetter,
};
use crate::api_impl::historical_overload::{
    historical_rpc_overloaded_error, is_historical_rpc_overloaded,
};
use crate::api_impl::utils::{build_arc_debank_traces, build_debank_traces};
use crate::error::{internal_rpc_err, rpc_error_with_code};

use alloy::rpc::types::state::StateOverride;
use alloy::sol_types::{decode_revert_reason, SolValue};
use jsonrpsee::{core::RpcResult, http_client::HttpClient};
use leafage_evm_storage::{BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{
    block_env_from_block, Address, BlockEnv, BlockId, BlockInfo, BlockNumberOrTag, BlockOverrides,
    BlockType, Bytes, CallRequest, DebankBlock, DebankBlockContext, DebankErrorCode,
    DebankMultiCallResp, DebankMultiCallStats, DebankSimulateResp, DebankSimulateStats,
    DebankSingleCallResult, DebankSingleSimulateResult, Header, JsonStorageKey, TransactionInfo,
    H256, KECCAK256_EMPTY, U256,
};
use revm::bytecode::OpCode;
use revm::context::result::InvalidTransaction;
use revm::context::result::{ExecutionResult, HaltReason};
use revm::context::{TransactTo, Transaction as TransactionTrait};
use revm::database::{CacheDB, DatabaseRef, DbAccount};
use revm::primitives::hardfork::SpecId as EthSpecId;
use revm_inspectors::tracing::{OpcodeFilter, TracingInspectorConfig};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::error;

fn estimate_gas_limit_cap(
    configured_rpc_cap: Option<u64>,
    consensus_cap: u64,
    block_gas_limit: u64,
) -> u64 {
    let rpc_cap = configured_rpc_cap
        .filter(|&cap| cap != 0)
        .unwrap_or(u64::MAX);
    consensus_cap.min(rpc_cap).min(block_gas_limit)
}

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
    pub(crate) fn should_try_historical(
        &self,
        block_ctx: &Option<DebankBlockContext>,
    ) -> Option<&HttpClient> {
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

    pub(crate) fn debank_get_state_by_ctx_impl(
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

    fn debank_get_latest_block_inner(&self) -> RpcResult<DebankBlock> {
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

    async fn debank_get_latest_block_impl(&self) -> RpcResult<DebankBlock> {
        let this = self.clone();
        utils::spawn_blocking_with_cancel(move |_token| this.debank_get_latest_block_inner())
            .await
            .map_err(|_| internal_rpc_err("get latest block failed"))?
    }

    fn debank_get_block_by_height_inner(&self, height: U256) -> RpcResult<DebankBlock> {
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

    async fn debank_get_block_by_height_impl(&self, height: U256) -> RpcResult<DebankBlock> {
        let this = self.clone();
        utils::spawn_blocking_with_cancel(move |_token| {
            this.debank_get_block_by_height_inner(height)
        })
        .await
        .map_err(|_| internal_rpc_err("get block by height failed"))?
    }

    fn debank_get_block_by_id_inner(&self, id: H256) -> RpcResult<DebankBlock> {
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

    async fn debank_get_block_by_id_impl(&self, id: H256) -> RpcResult<DebankBlock> {
        let this = self.clone();
        utils::spawn_blocking_with_cancel(move |_token| this.debank_get_block_by_id_inner(id))
            .await
            .map_err(|_| internal_rpc_err("get block by id failed"))?
    }

    fn debank_get_address_nonce_inner(
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

    async fn debank_get_address_nonce_impl(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<U256> {
        let limiter = self.inner.evm_cfg().state_read_limiter.clone();
        let this = self.clone();
        utils::spawn_blocking_limited_with_cancel(limiter, move |_token| {
            this.debank_get_address_nonce_inner(address, block_ctx)
        })
        .await
        .map_err(|_| internal_rpc_err("get address nonce failed"))?
    }

    fn debank_get_address_balance_inner(
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
        let balance = account.map(|a| a.balance);
        Ok(U256::from(balance.unwrap_or_default()))
    }

    async fn debank_get_address_balance_impl(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<U256> {
        // The virtual balance answers without any state read: don't
        // spend a state-read permit or a blocking-pool slot on it.
        if let Some(vb) = self.inner.virtual_balance() {
            return Ok(vb);
        }
        let limiter = self.inner.evm_cfg().state_read_limiter.clone();
        let this = self.clone();
        utils::spawn_blocking_limited_with_cancel(limiter, move |_token| {
            this.debank_get_address_balance_inner(address, block_ctx)
        })
        .await
        .map_err(|_| internal_rpc_err("get address balance failed"))?
    }

    fn debank_get_storage_at_inner(
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

    async fn debank_get_storage_at_impl(
        &self,
        address: Address,
        index: H256,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<H256> {
        let limiter = self.inner.evm_cfg().state_read_limiter.clone();
        let this = self.clone();
        utils::spawn_blocking_limited_with_cancel(limiter, move |_token| {
            this.debank_get_storage_at_inner(address, index, block_ctx)
        })
        .await
        .map_err(|_| internal_rpc_err("get address storage failed"))?
    }

    fn debank_get_code_inner(
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
    async fn debank_get_code_impl(
        &self,
        address: Address,
        block_ctx: Option<DebankBlockContext>,
    ) -> RpcResult<Bytes> {
        let limiter = self.inner.evm_cfg().state_read_limiter.clone();
        let this = self.clone();
        utils::spawn_blocking_limited_with_cancel(limiter, move |_token| {
            this.debank_get_code_inner(address, block_ctx)
        })
        .await
        .map_err(|_| internal_rpc_err("get address code failed"))?
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
        block: &BlockInfo,
        block_env: &BlockEnv,
        db: &utils::RequestCacheDB<EvmStorageWrapper<<C::DB as EvmStorageRead>::StateDB>>,
        request: CallRequest,
    ) -> RpcResult<DebankSingleCallResult> {
        let start = std::time::Instant::now();

        // Collect ERC20 token address if token_collector is enabled
        if let Some(collector) = self.inner.token_collector() {
            let to = request.to.and_then(|txkind| txkind.to().copied());
            let data = request.input.input().map(|d| d.as_ref()).unwrap_or(&[]);
            collector.maybe_collect_call(to, data);
        }

        if let Some(txkind) = request.to {
            if let Some(address) = txkind.to() {
                if *address == *utils::NATIVE_TOKEN_SENTINEL {
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
        let (call_block_env, tx) = self.inner.create_txn_env_for_call(
            block,
            block_env.clone(),
            request,
            db,
            self.inner.evm_cfg().cfg.chain_id,
        )?;
        let mut res: DebankSingleCallResult = self
            .inner
            .transact_for_call(&call_block_env, db, tx)
            .map_err(|e| e.to_rpc_error())?
            .into();
        res.time_cost = start.elapsed().as_secs_f64();
        Ok(res)
    }

    /// Warms `cache_db` with every call's `from`/`to` account and the
    /// deduplicated contract code behind them, using one batched read
    /// per kind instead of a layered-state walk per first touch during
    /// the serial call loop. Entries already in the cache (state
    /// overrides, block overrides) are never replaced, the native-token
    /// sentinel is skipped because its calls bypass the EVM, and any
    /// prefetch error is dropped so the affected keys fall back to the
    /// on-demand scalar path with its unchanged per-call error text.
    /// Nonexistent accounts get a negative cache entry — without it the
    /// call loop would re-read them through the scalar path, making the
    /// prefetch a net extra read for fresh addresses.
    fn prefetch_multi_call_accounts(
        requests: &[CallRequest],
        cache_db: &mut CacheDB<EvmStorageWrapper<<C::DB as EvmStorageRead>::StateDB>>,
        cancel_token: &CancellationToken,
    ) {
        // leafage-py chunks multicalls at 20 calls, so real traffic
        // always fits this window; on oversized batches the tail keeps
        // lazy on-demand reads instead of growing the eager work done
        // ahead of the loop's fast_fail / cancellation checks.
        const MAX_PREFETCH_CALLS: usize = 32;
        let mut addresses: Vec<Address> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for request in requests.iter().take(MAX_PREFETCH_CALLS) {
            let to = request.to.and_then(|txkind| txkind.to().copied());
            for address in request.from.into_iter().chain(to) {
                if address == *utils::NATIVE_TOKEN_SENTINEL {
                    continue;
                }
                if !cache_db.cache.accounts.contains_key(&address) && seen.insert(address) {
                    addresses.push(address);
                }
            }
        }
        if addresses.is_empty() {
            return;
        }
        let Ok(infos) = cache_db.db.basic_many_ref(&addresses) else {
            return;
        };
        if cancel_token.is_cancelled() {
            return;
        }
        let mut code_hashes: Vec<H256> = Vec::new();
        let mut seen_hashes = std::collections::HashSet::new();
        for info in infos.iter().flatten() {
            let code_hash = info.code_hash;
            if code_hash.is_zero() || code_hash == KECCAK256_EMPTY {
                continue;
            }
            if !cache_db.cache.contracts.contains_key(&code_hash) && seen_hashes.insert(code_hash) {
                code_hashes.push(code_hash);
            }
        }
        if !code_hashes.is_empty() {
            if let Ok(codes) = cache_db.db.code_by_hash_many_ref(&code_hashes) {
                for (code_hash, code) in code_hashes.into_iter().zip(codes) {
                    cache_db.cache.contracts.insert(code_hash, code);
                }
            }
        }
        for (address, info) in addresses.into_iter().zip(infos) {
            // Mirrors `CacheDB::load_account`'s construction byte for
            // byte: `insert_account_info` would rewrite a raw zero
            // `code_hash` to `KECCAK_EMPTY`, making prefetched accounts
            // observably differ from lazily loaded ones (e.g. through
            // EXTCODEHASH) — the store keeps zero hashes for EOAs.
            let entry = match info {
                Some(info) => DbAccount {
                    info,
                    ..Default::default()
                },
                None => DbAccount::new_not_existing(),
            };
            cache_db.cache.accounts.insert(address, entry);
        }
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
        // Block env, overrides and the request-scoped read cache are
        // shared by every call in this multicall: overrides apply once,
        // and repeated keys across calls skip the layered-state walk.
        let mut block_env = block_env_from_block(&block);
        let mut cache_db = CacheDB::new(EvmStorageWrapper {
            db: state.clone(),
            ovm_address: self.inner.evm_cfg().ovm_address.clone(),
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        });
        if let Some(overrides) = block_overrides {
            super::utils::apply_block_overrides(
                overrides,
                &mut cache_db,
                &mut block_env,
                block.header.clone(),
            );
        }
        if let Some(state_override) = state_override {
            if self.inner.arc_chain_config().is_some() {
                super::utils::apply_state_overrides_arc_debank(state_override, &mut cache_db)?;
            } else {
                super::utils::apply_state_overrides(state_override, &mut cache_db)?;
            }
        }
        // Prefetch only where the batched reads are real: archive and
        // MDBX backends fall back to scalar loops, and OVM chains force
        // scalar account reads, so prefetching there just front-loads
        // the same point reads — under fast_fail (the SDK default) an
        // early failure would turn that into O(N) wasted reads. On
        // MultiGet backends the fast_fail waste is bounded by two
        // batched reads over the capped prefetch window.
        if cache_db.db.supports_batched_reads() {
            Self::prefetch_multi_call_accounts(&requests, &mut cache_db, &cancel_token);
        }
        let db = utils::RequestCacheDB::new(cache_db);
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
                &state, &block, &block_env, &db, request,
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
        let limiter = self.inner.evm_cfg().exec_limiter.clone();
        let this = self.clone();
        utils::spawn_blocking_limited_with_cancel(limiter, move |token| {
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
        let limiter = self.inner.evm_cfg().exec_limiter.clone();
        let this = self.clone();
        utils::spawn_blocking_limited_with_cancel(limiter, move |token| {
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
        let is_arc = self.inner.arc_chain_config().is_some();
        let environment =
            self.inner
                .prepare_query_environment(&block, block_overrides, &mut memory_db)?;
        let block_env = environment.block_env;
        if let Some(header) = environment.pre_execution_header {
            self.inner
                .apply_pre_execution_changes(header, &block_env, &mut memory_db)?;
        }
        let mut results: Vec<DebankSingleSimulateResult> = Vec::new();
        for (tx_index, tx) in (0_u64..).zip(txs) {
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
            if let Some(last_res) = results.last() {
                if last_res.code != 0 {
                    results.push(last_res.clone());
                    continue;
                }
            }
            let mut trace_cfg = TracingInspectorConfig::default_parity()
                .set_record_logs(true)
                .set_steps(true);
            if is_arc {
                trace_cfg = trace_cfg.set_exclude_precompile_calls(false);
            }
            trace_cfg.record_opcodes_filter = Some(OpcodeFilter::new().enabled(OpCode::SSTORE));
            let tx = self.inner.create_txn_env_for_simulation(
                &block,
                &block_env,
                tx,
                &memory_db,
                self.inner.evm_cfg().cfg.chain_id,
            )?;
            let (exec_res, trace_arena, log_emitters) = self
                .inner
                .inspect_tx_commit_for_simulation(
                    &block_env,
                    &mut memory_db,
                    trace_cfg,
                    |inspector| inspector.into_traces(),
                    tx,
                )
                .map_err(|e| e.to_rpc_error())?;
            let (traces, mut events) = if is_arc {
                build_arc_debank_traces(tx_info.hash.unwrap(), trace_arena, &log_emitters)
            } else {
                build_debank_traces(tx_info.hash.unwrap(), trace_arena)
            };
            if is_arc && !exec_res.is_success() {
                // The inspector observes frame-init and opcode logs before the
                // enclosing frame outcome is known. A failed top-level Arc
                // transaction has no consensus logs, so do not expose those
                // reverted observations as DeBank events.
                events.clear();
            }
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
        let mut cache_db = CacheDB::new(EvmStorageWrapper {
            db: state,
            ovm_address: self.inner.evm_cfg().ovm_address.clone(),
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        });
        if let Some(overrides) = block_overrides.clone() {
            utils::apply_block_overrides(
                overrides,
                &mut cache_db,
                &mut block_env,
                block.header.clone(),
            );
        }
        // The binary search below re-executes the same tx many times;
        // the request-scoped cache lets every retry after the first read
        // its state from memory instead of re-walking the layered state.
        let memory_db = utils::RequestCacheDB::new(cache_db);
        // Keep a copy of gas related request values
        let tx_request_gas_limit = request.gas;
        // the gas limit of the corresponding block
        let block_env_gas_limit = block_env.gas_limit;
        let cfg = &self.inner.evm_cfg().cfg;
        // Keep the configured RPC cap separate from the chain's consensus cap.
        // Cfg::tx_gas_limit_cap() cannot be used here because it derives the
        // Ethereum EIP-7825 cap from Osaka when the raw field is None; Arbitrum
        // is explicitly exempt and enforces its state-derived limit in its handler.
        let chain_spec: EthSpecId = cfg.spec().clone().into();
        let consensus_cap = self.inner.consensus_tx_gas_limit_cap(chain_spec);
        let max_gas_limit =
            estimate_gas_limit_cap(cfg.tx_gas_limit_cap, consensus_cap, block_env_gas_limit);
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
            &block,
            &block_env,
            request.clone(),
            &memory_db,
            self.inner.evm_cfg().cfg.chain_id,
        )?;
        tx.set_gas_estimation();
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
                                let l1_overhead = self
                                    .inner
                                    .estimate_l1_overhead(&block, &block_env, tx, &memory_db);
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
            .map_err(|error| error.to_rpc_error())?;

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

        // https://github.com/paradigmxyz/reth/pull/16413
        while (lowest_gas_limit + 1) < highest_gas_limit {
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
                            invalid_tx_err => {
                                return Err(rpc_error_with_code(
                                    DebankErrorCode::EvmFailed as i32,
                                    format!("Invalid transaction: {:?}", invalid_tx_err),
                                ));
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
        let l1_overhead =
            self.inner
                .estimate_l1_overhead(&block, &block_env, tx.clone(), &memory_db);

        Ok(U256::from(final_gas.saturating_add(l1_overhead)))
    }

    async fn debank_estimate_gas_impl(
        &self,
        request: CallRequest,
        block_ctx: Option<DebankBlockContext>,
        block_overrides: Option<BlockOverrides>,
    ) -> RpcResult<U256> {
        let limiter = self.inner.evm_cfg().exec_limiter.clone();
        let this = self.clone();
        utils::spawn_blocking_limited_with_cancel(limiter, move |token| {
            this.debank_estimate_gas_inner(request, block_ctx, block_overrides, token)
        })
        .await
        .inspect_err(|err| error!("Failed to spawn debank_estimate result: {:?}", err))
        .map_err(|_| internal_rpc_err("estimate failed".to_string()))?
    }

    fn block_is_valid_inner(&self, id: H256) -> RpcResult<bool> {
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

    async fn block_is_valid_impl(&self, id: H256) -> RpcResult<bool> {
        let this = self.clone();
        utils::spawn_blocking_with_cancel(move |_token| this.block_is_valid_inner(id))
            .await
            .map_err(|_| internal_rpc_err("block is valid failed"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::EthApiServer;
    use crate::api_impl::ApiImpl;
    use alloy::eips::eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE};
    use alloy::eips::eip7702::Authorization;
    use alloy::primitives::{hex, keccak256};
    use alloy::rpc::types::state::AccountOverride;
    use alloy::rpc::types::{TransactionInput, TransactionRequest};
    use alloy::signers::{local::PrivateKeySigner, SignerSync};
    use leafage_evm_chains::arc::{ArcChainConfig, ARC_MAINNET_CHAIN_ID};
    use leafage_evm_storage::{
        EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
        StorageKind,
    };
    use leafage_evm_types::{
        AccountStorageDiff, Block, BlockStorageDiff, CfgEnv, DebankID, IndexValuePair,
        MainnetSpecId, NewAccount, NewCode,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    type ArcTestApi = Api<ApiImpl<Arc<StateTree<MultiStorage>>, MainnetSpecId, ArcChainConfig>>;

    const ARC_RPC_GAS_CAP: u64 = 25_000_000;
    const BLOCK_GAS_LIMIT: u64 = 30_000_000;
    const ANCHOR_NUMBER: u64 = 1;
    const ANCHOR_BASE_FEE: u64 = 3;
    const NEXT_BASE_FEE: u64 = 7;

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Copy)]
    struct TestAddresses {
        funded: Address,
        native_fiat_token: Address,
        observer: Address,
        limited: Address,
        fee_limited: Address,
        empty: Address,
        blocked: Address,
        gas_guard: Address,
        block_gas_guard: Address,
        rpc_cap_guard: Address,
        revert: Address,
        environment: Address,
        counter: Address,
        logger: Address,
        logger_revert: Address,
        delegate_proxy: Address,
        delegate_implementation: Address,
        balance_reader: Address,
        beneficiary: Address,
        selfdestruct_revert_parent: Address,
        selfdestruct_nonzero: Address,
        selfdestruct_zero: Address,
        selfdestruct_beneficiary: Address,
        reverted_child_then_selfdestruct: Address,
    }

    struct ArcFixture {
        api: ArcTestApi,
        addresses: TestAddresses,
        path: PathBuf,
    }

    impl ArcFixture {
        fn close(self) {
            let path = self.path.clone();
            drop(self);
            let _ = std::fs::remove_dir_all(path);
        }
    }

    fn test_block(number: u64, hash: H256, parent_hash: H256, next_base_fee: u64) -> BlockInfo {
        let mut block = BlockInfo {
            inner: Block::empty(Default::default()),
            other: Default::default(),
        };
        block.inner.header.hash = hash;
        block.inner.header.inner.number = number;
        block.inner.header.inner.parent_hash = parent_hash;
        block.inner.header.inner.timestamp = 1_000 + number;
        block.inner.header.inner.gas_limit = BLOCK_GAS_LIMIT;
        block.inner.header.inner.base_fee_per_gas = Some(ANCHOR_BASE_FEE);
        block.inner.header.inner.extra_data = Bytes::copy_from_slice(&next_base_fee.to_be_bytes());
        block.inner.header.inner.beneficiary = Address::repeat_byte(0x77);
        block.inner.header.inner.mix_hash = H256::repeat_byte(0x44);
        block.inner.header.inner.excess_blob_gas = Some(0);
        block.inner.header.inner.blob_gas_used = Some(0);
        block
    }

    fn gas_guard_code(threshold: u32) -> Bytes {
        let threshold = threshold.to_be_bytes();
        Bytes::from(vec![
            // Revert when GAS at contract entry is below `threshold`.
            0x5a,
            0x63,
            threshold[0],
            threshold[1],
            threshold[2],
            threshold[3],
            0x90,
            0x10,
            0x60,
            0x0c,
            0x57,
            0x00,
            0x5b,
            0x5f,
            0x5f,
            0xfd,
        ])
    }

    fn environment_code() -> Bytes {
        Bytes::from_static(&[
            // Return NUMBER, BASEFEE, BLOCKHASH(0), and BLOCKHASH(1).
            0x43, 0x5f, 0x52, 0x48, 0x60, 0x20, 0x52, 0x5f, 0x40, 0x60, 0x40, 0x52, 0x60, 0x01,
            0x40, 0x60, 0x60, 0x52, 0x60, 0x80, 0x5f, 0xf3,
        ])
    }

    fn counter_code() -> Bytes {
        Bytes::from_static(&[
            // storage[0] += 1; return storage[0].
            0x5f, 0x54, 0x60, 0x01, 0x01, 0x80, 0x5f, 0x55, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
        ])
    }

    fn logger_code() -> Bytes {
        // Emit one ordinary LOG0 from the execution address.
        Bytes::from_static(&[0x5f, 0x5f, 0xa0, 0x00])
    }

    fn logger_revert_code() -> Bytes {
        // Emit LOG0 and then revert, so both this log and Arc's frame-init
        // EIP-7708 log must disappear from the simulated result.
        Bytes::from_static(&[0x5f, 0x5f, 0xa0, 0x5f, 0x5f, 0xfd])
    }

    fn delegate_proxy_code(implementation: Address) -> Bytes {
        let mut code = vec![0x5f, 0x5f, 0x5f, 0x5f, 0x73];
        code.extend_from_slice(implementation.as_slice());
        code.extend_from_slice(&[0x5a, 0xf4, 0x50, 0x00]);
        code.into()
    }

    fn balance_reader_code() -> Bytes {
        Bytes::from_static(&[
            // return BALANCE(address(calldataload(0))).
            0x5f, 0x35, 0x31, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
        ])
    }

    fn init_code(runtime: &[u8]) -> Bytes {
        assert!(runtime.len() <= u8::MAX as usize);
        let mut init = vec![
            0x60,
            runtime.len() as u8,
            0x60,
            0x0c,
            0x60,
            0x00,
            0x39,
            0x60,
            runtime.len() as u8,
            0x60,
            0x00,
            0xf3,
        ];
        init.extend_from_slice(runtime);
        init.into()
    }

    fn native_coin_mint_input(to: Address, amount: U256) -> Bytes {
        let mut input = selector("mint(address,uint256)").to_vec();
        input.extend_from_slice(&address_word(to));
        input.extend_from_slice(&amount.to_be_bytes::<32>());
        input.into()
    }

    fn native_fiat_token_code(account: Address) -> Bytes {
        let native_coin_control: Address = "0x1800000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let mut input_word = [0u8; 32];
        input_word[..4].copy_from_slice(&selector("blocklist(address)"));

        let mut code = vec![0x7f];
        code.extend_from_slice(&input_word);
        code.extend_from_slice(&[0x5f, 0x52, 0x73]);
        code.extend_from_slice(account.as_slice());
        code.extend_from_slice(&[
            0x60, 0x04, 0x52, 0x60, 0x20, 0x5f, 0x60, 0x24, 0x5f, 0x5f, 0x73,
        ]);
        code.extend_from_slice(native_coin_control.as_slice());
        code.extend_from_slice(&[0x5a, 0xf1, 0x50, 0x60, 0x20, 0x5f, 0xf3]);
        code.into()
    }

    fn selfdestruct_code(beneficiary: Address) -> Bytes {
        let mut code = vec![0x73];
        code.extend_from_slice(beneficiary.as_slice());
        code.push(0xff);
        code.into()
    }

    fn call_child_code(child: Address, revert: bool) -> Bytes {
        // CALL the child with zero value/input/output and consume the success
        // word. The reverting variant then rolls the successful child frame
        // back from its parent.
        let mut code = vec![0x5f, 0x5f, 0x5f, 0x5f, 0x5f, 0x73];
        code.extend_from_slice(child.as_slice());
        code.extend_from_slice(&[0x5a, 0xf1, 0x50]);
        if revert {
            code.extend_from_slice(&[0x5f, 0x5f, 0xfd]);
        } else {
            code.push(0x00);
        }
        code.into()
    }

    fn reverted_value_child_then_selfdestruct_code(child: Address, beneficiary: Address) -> Bytes {
        // The child receives value (Arc EIP-7708), emits LOG0, and reverts.
        // After its checkpoint is rolled back, the parent SELFDESTRUCT writes
        // a direct Arc EIP-7708 log that must still reach the inspector.
        let mut code = vec![0x5f, 0x5f, 0x5f, 0x5f, 0x60, 0x05, 0x73];
        code.extend_from_slice(child.as_slice());
        code.extend_from_slice(&[0x5a, 0xf1, 0x50, 0x73]);
        code.extend_from_slice(beneficiary.as_slice());
        code.push(0xff);
        code.into()
    }

    fn blocklist_storage_index(address: Address) -> H256 {
        let mut mapping_input = [0u8; 64];
        mapping_input[12..32].copy_from_slice(address.as_slice());
        mapping_input[63] = 2;
        keccak256(keccak256(mapping_input))
    }

    fn build_arc_fixture(estimate_gas_buffer: u64) -> ArcFixture {
        build_arc_fixture_with_rpc_gas_cap(estimate_gas_buffer, ARC_RPC_GAS_CAP)
    }

    fn build_arc_fixture_with_rpc_gas_cap(
        estimate_gas_buffer: u64,
        rpc_gas_cap: u64,
    ) -> ArcFixture {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("leafage-arc-estimate-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();

        let addresses = TestAddresses {
            funded: Address::repeat_byte(0x11),
            native_fiat_token: "0x3600000000000000000000000000000000000000"
                .parse()
                .unwrap(),
            observer: Address::repeat_byte(0x13),
            limited: Address::repeat_byte(0x12),
            fee_limited: Address::repeat_byte(0x14),
            empty: Address::repeat_byte(0x22),
            blocked: Address::repeat_byte(0x33),
            gas_guard: Address::repeat_byte(0x44),
            block_gas_guard: Address::repeat_byte(0x76),
            rpc_cap_guard: Address::repeat_byte(0x78),
            revert: Address::repeat_byte(0x55),
            environment: Address::repeat_byte(0x66),
            counter: Address::repeat_byte(0x68),
            logger: Address::repeat_byte(0x69),
            logger_revert: Address::repeat_byte(0x6d),
            delegate_proxy: Address::repeat_byte(0x6a),
            delegate_implementation: Address::repeat_byte(0x6b),
            balance_reader: Address::repeat_byte(0x6c),
            beneficiary: Address::repeat_byte(0x77),
            selfdestruct_revert_parent: Address::repeat_byte(0x72),
            selfdestruct_nonzero: Address::repeat_byte(0x73),
            selfdestruct_zero: Address::repeat_byte(0x74),
            selfdestruct_beneficiary: Address::repeat_byte(0x75),
            reverted_child_then_selfdestruct: Address::repeat_byte(0x79),
        };
        let native_fiat_token = native_fiat_token_code(addresses.empty);
        let native_coin_control: Address = "0x1800000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let gas_guard = gas_guard_code(300_000);
        let block_gas_guard = gas_guard_code((BLOCK_GAS_LIMIT + 1).try_into().unwrap());
        let rpc_cap_guard = gas_guard_code(45_000_000);
        let revert = Bytes::from_static(&[0x5f, 0x5f, 0xfd]);
        let environment = environment_code();
        let counter = counter_code();
        let logger = logger_code();
        let logger_revert = logger_revert_code();
        let delegate_implementation = logger_code();
        let delegate_proxy = delegate_proxy_code(addresses.delegate_implementation);
        let balance_reader = balance_reader_code();
        let selfdestruct = selfdestruct_code(addresses.selfdestruct_beneficiary);
        let selfdestruct_revert_parent = call_child_code(addresses.selfdestruct_nonzero, true);
        let reverted_child_then_selfdestruct = reverted_value_child_then_selfdestruct_code(
            addresses.logger_revert,
            addresses.selfdestruct_beneficiary,
        );
        let history_storage = HISTORY_STORAGE_CODE.clone();

        let mut diff = BlockStorageDiff::default();
        for (address, balance, nonce, code_hash) in [
            (addresses.funded, U256::ONE << 128, 0, H256::ZERO),
            (
                addresses.native_fiat_token,
                U256::ONE << 128,
                0,
                keccak256(&native_fiat_token),
            ),
            (addresses.observer, U256::ONE << 128, 0, H256::ZERO),
            (addresses.limited, U256::from(250_000u64), 0, H256::ZERO),
            (
                addresses.fee_limited,
                U256::from(2_000_000u64),
                0,
                H256::ZERO,
            ),
            (
                addresses.blocked,
                U256::from(1_000_000_000u64),
                0,
                H256::ZERO,
            ),
            (native_coin_control, U256::ZERO, 1, H256::ZERO),
            (addresses.gas_guard, U256::ZERO, 1, keccak256(&gas_guard)),
            (
                addresses.block_gas_guard,
                U256::ZERO,
                1,
                keccak256(&block_gas_guard),
            ),
            (
                addresses.rpc_cap_guard,
                U256::ZERO,
                1,
                keccak256(&rpc_cap_guard),
            ),
            (addresses.revert, U256::ZERO, 1, keccak256(&revert)),
            (
                addresses.environment,
                U256::ZERO,
                1,
                keccak256(&environment),
            ),
            (addresses.counter, U256::ZERO, 1, keccak256(&counter)),
            (addresses.logger, U256::ZERO, 1, keccak256(&logger)),
            (
                addresses.logger_revert,
                U256::ZERO,
                1,
                keccak256(&logger_revert),
            ),
            (
                addresses.delegate_proxy,
                U256::ZERO,
                1,
                keccak256(&delegate_proxy),
            ),
            (
                addresses.delegate_implementation,
                U256::ZERO,
                1,
                keccak256(&delegate_implementation),
            ),
            (
                addresses.balance_reader,
                U256::ZERO,
                1,
                keccak256(&balance_reader),
            ),
            (
                addresses.selfdestruct_revert_parent,
                U256::ZERO,
                1,
                keccak256(&selfdestruct_revert_parent),
            ),
            (
                addresses.selfdestruct_nonzero,
                U256::from(42),
                1,
                keccak256(&selfdestruct),
            ),
            (
                addresses.selfdestruct_zero,
                U256::ZERO,
                1,
                keccak256(&selfdestruct),
            ),
            (
                addresses.reverted_child_then_selfdestruct,
                U256::from(10),
                1,
                keccak256(&reverted_child_then_selfdestruct),
            ),
            (
                HISTORY_STORAGE_ADDRESS,
                U256::ZERO,
                1,
                keccak256(&history_storage),
            ),
        ] {
            diff.new_accounts.push(NewAccount {
                address: keccak256(address.as_slice()),
                balance,
                nonce,
                code_hash,
            });
        }
        diff.new_codes.extend([
            NewCode {
                code_hash: keccak256(&native_fiat_token),
                code: native_fiat_token,
            },
            NewCode {
                code_hash: keccak256(&gas_guard),
                code: gas_guard,
            },
            NewCode {
                code_hash: keccak256(&block_gas_guard),
                code: block_gas_guard,
            },
            NewCode {
                code_hash: keccak256(&rpc_cap_guard),
                code: rpc_cap_guard,
            },
            NewCode {
                code_hash: keccak256(&revert),
                code: revert,
            },
            NewCode {
                code_hash: keccak256(&environment),
                code: environment,
            },
            NewCode {
                code_hash: keccak256(&counter),
                code: counter,
            },
            NewCode {
                code_hash: keccak256(&logger),
                code: logger,
            },
            NewCode {
                code_hash: keccak256(&logger_revert),
                code: logger_revert,
            },
            NewCode {
                code_hash: keccak256(&delegate_proxy),
                code: delegate_proxy,
            },
            NewCode {
                code_hash: keccak256(&delegate_implementation),
                code: delegate_implementation,
            },
            NewCode {
                code_hash: keccak256(&balance_reader),
                code: balance_reader,
            },
            NewCode {
                code_hash: keccak256(&selfdestruct),
                code: selfdestruct,
            },
            NewCode {
                code_hash: keccak256(&selfdestruct_revert_parent),
                code: selfdestruct_revert_parent,
            },
            NewCode {
                code_hash: keccak256(&reverted_child_then_selfdestruct),
                code: reverted_child_then_selfdestruct,
            },
            NewCode {
                code_hash: keccak256(&history_storage),
                code: history_storage,
            },
        ]);
        diff.storage_diffs.push(AccountStorageDiff {
            address: keccak256(native_coin_control.as_slice()),
            diffs: vec![IndexValuePair {
                index: blocklist_storage_index(addresses.blocked),
                value: U256::ONE,
            }],
        });

        let db = MultiStorage::open(&path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
        let mut genesis = test_block(0, H256::repeat_byte(0xaa), H256::ZERO, ANCHOR_BASE_FEE);
        // Arc block 0 predates the eight-byte nextBaseFee extension. Its H+1
        // query therefore exercises the typed static fee-config fallback.
        genesis.inner.header.inner.base_fee_per_gas = Some(1_000_000_000);
        genesis.inner.header.inner.gas_used = 0;
        genesis.inner.header.inner.extra_data = Bytes::new();
        StateDBWrapper(
            db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                .unwrap()
                .unwrap(),
        )
        .update_block(genesis, diff)
        .unwrap();

        let tree =
            Arc::new(StateTree::new(db, StateTreeConfig::new(4, 1000, 1000, 1000, true)).unwrap());
        tree.update_block(
            test_block(
                ANCHOR_NUMBER,
                H256::repeat_byte(0xbb),
                H256::repeat_byte(0xaa),
                NEXT_BASE_FEE,
            ),
            BlockStorageDiff::default(),
        )
        .unwrap();

        let arc_config = ArcChainConfig::mainnet();
        let mut cfg = CfgEnv::new_with_spec(arc_config.ethereum_spec());
        cfg.chain_id = ARC_MAINNET_CHAIN_ID;
        cfg.tx_gas_limit_cap = Some(rpc_gas_cap);
        let api = Api::new(ApiImpl::new(
            tree,
            cfg,
            Some(arc_config),
            None,
            None,
            None,
            false,
            false,
            "arc-estimate-test".to_string(),
            estimate_gas_buffer,
            None,
            None,
            None,
        ));

        ArcFixture {
            api,
            addresses,
            path,
        }
    }

    fn anchor_context() -> Option<DebankBlockContext> {
        Some(DebankBlockContext {
            block_id: BlockId::Number(BlockNumberOrTag::Number(ANCHOR_NUMBER)),
            block_type: BlockType::Equals,
        })
    }

    fn call_request(from: Address, to: Address) -> CallRequest {
        CallRequest {
            inner: TransactionRequest::default().from(from).to(to),
            tempo: None,
        }
    }

    fn estimate(
        api: &ArcTestApi,
        request: CallRequest,
        overrides: Option<BlockOverrides>,
    ) -> RpcResult<U256> {
        api.debank_estimate_gas_inner(
            request,
            anchor_context(),
            overrides,
            CancellationToken::new(),
        )
    }

    fn execute_arc_estimate_probe(
        api: &ArcTestApi,
        mut request: CallRequest,
        overrides: Option<BlockOverrides>,
        gas_limit: u64,
    ) -> ExecutionResult<HaltReason> {
        let state = api.debank_get_state_by_ctx_impl(anchor_context()).unwrap();
        let block = state.block_info_arc().unwrap();
        request.nonce = None;
        request.gas = Some(gas_limit);
        let mut cache_db = CacheDB::new(EvmStorageWrapper {
            db: state,
            ovm_address: None,
            normalize_state_key: false,
        });
        let mut block_env = block_env_from_block(&block);
        if let Some(overrides) = overrides {
            utils::apply_block_overrides(
                overrides,
                &mut cache_db,
                &mut block_env,
                block.header.clone(),
            );
        }
        let memory_db = utils::RequestCacheDB::new(cache_db);
        let mut tx = api
            .inner
            .create_txn_env(
                &block,
                &block_env,
                request,
                &memory_db,
                ARC_MAINNET_CHAIN_ID,
            )
            .unwrap();
        tx.set_gas_estimation();
        tx.set_gas_limit(gas_limit);
        api.inner.transact(&block_env, &memory_db, tx).unwrap()
    }

    fn success_output(result: ExecutionResult<HaltReason>) -> Bytes {
        match result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            other => panic!("expected success, got {other:?}"),
        }
    }

    fn output_words(output: &Bytes) -> Vec<U256> {
        output.chunks_exact(32).map(U256::from_be_slice).collect()
    }

    fn request_with_input(from: Address, to: Address, input: Bytes) -> CallRequest {
        CallRequest {
            inner: TransactionRequest::default()
                .from(from)
                .to(to)
                .input(TransactionInput::new(input)),
            tempo: None,
        }
    }

    fn code_override(address: Address, code: Bytes) -> StateOverride {
        let mut override_state = StateOverride::default();
        override_state.insert(address, AccountOverride::default().with_code(code));
        override_state
    }

    fn selector(signature: &str) -> Bytes {
        Bytes::copy_from_slice(&keccak256(signature.as_bytes())[..4])
    }

    fn address_word(address: Address) -> Bytes {
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(address.as_slice());
        Bytes::copy_from_slice(&word)
    }

    fn p256_valid_input() -> Bytes {
        // Daimo P256 verifier vector, also checked into revm-precompile 32.1.0.
        hex::decode("4cee90eb86eaa050036147a12d49004b6b9c72bd725d39d4785011fe190f0b4da73bd4903f0ce3b639bbbf6e8e80d16931ff4bcf5993d58468e8fb19086e8cac36dbcd03009df8c59286b162af3bd7fcc0450c9aa81be5d10d312af6c66b1d604aebd3099c618202fcfe16ae7770b0c49ab5eadf74b754204a3bb6060e44eff37618b065f9832de4ca6ca971a7a1adc826d0f7c00181a5fb2ddf79ae00b4e10e")
            .unwrap()
            .into()
    }

    fn root_trace_output(result: &DebankSingleSimulateResult) -> Bytes {
        result
            .traces
            .first()
            .expect("top-level trace")
            .output
            .clone()
    }

    fn assert_arc_transfer_event(
        event: &leafage_evm_types::DebankEvent,
        from: Address,
        to: Address,
        amount: U256,
        parent_trace_id: &str,
        pos_in_parent_trace: usize,
    ) {
        assert_eq!(
            event.contract_id,
            "0xfffffffffffffffffffffffffffffffffffffffe"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(
            event.selector,
            keccak256("Transfer(address,address,uint256)").to_string()
        );
        assert_eq!(
            event.topics,
            vec![
                H256::left_padding_from(from.as_slice()).to_string(),
                H256::left_padding_from(to.as_slice()).to_string(),
            ]
        );
        assert_eq!(
            event.data,
            Bytes::copy_from_slice(&amount.to_be_bytes::<32>())
        );
        assert_eq!(event.parent_trace_id, parent_trace_id);
        assert_eq!(event.pos_in_parent_trace, pos_in_parent_trace);
        assert_eq!(event.id, event.debank_id());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_call_uses_typed_environment_call_policy_and_existing_overrides() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let anchor = BlockId::Number(BlockNumberOrTag::Number(ANCHOR_NUMBER));

        let historical = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.environment)
                .gas_price(ANCHOR_BASE_FEE as u128)
                // call-like semantics ignore an explicitly supplied nonce.
                .nonce(999),
            tempo: None,
        };
        let output = fixture
            .api
            .call(historical.clone(), anchor, None, None)
            .await
            .unwrap();
        let words = output_words(&output);
        assert_eq!(words[0], U256::from(ANCHOR_NUMBER));
        assert_eq!(words[1], U256::from(ANCHOR_BASE_FEE));
        assert_eq!(
            words[2],
            U256::from_be_slice(H256::repeat_byte(0xaa).as_slice())
        );
        assert_eq!(words[3], U256::ZERO);

        let overridden_hash = H256::repeat_byte(0xcc);
        let next = BlockOverrides {
            number: Some(U256::from(ANCHOR_NUMBER + 1)),
            base_fee: Some(U256::from(NEXT_BASE_FEE)),
            block_hash: Some(BTreeMap::from([(ANCHOR_NUMBER, overridden_hash)])),
            ..Default::default()
        };
        let next_request = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.environment)
                .gas_price(NEXT_BASE_FEE as u128),
            tempo: None,
        };
        let output = fixture
            .api
            .call(next_request, anchor, None, Some(next))
            .await
            .unwrap();
        let words = output_words(&output);
        assert_eq!(words[0], U256::from(ANCHOR_NUMBER + 1));
        assert_eq!(words[1], U256::from(NEXT_BASE_FEE));
        assert_eq!(words[3], U256::from_be_slice(overridden_hash.as_slice()));

        let zero_price = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.environment)
                .gas_price(0),
            tempo: None,
        };
        let output = fixture
            .api
            .call(zero_price, anchor, None, None)
            .await
            .unwrap();
        assert_eq!(output_words(&output)[1], U256::ZERO);

        let low_allowance = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.limited)
                .to(addresses.gas_guard)
                .gas_price(1),
            tempo: None,
        };
        assert!(fixture
            .api
            .call(low_allowance, anchor, None, None)
            .await
            .is_err());
        let explicit_gas = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.limited)
                .to(addresses.gas_guard)
                .gas_limit(400_000)
                .gas_price(1),
            tempo: None,
        };
        let explicit_gas = fixture
            .api
            .call(explicit_gas, anchor, None, None)
            .await
            .unwrap_err();
        assert_eq!(explicit_gas.code(), -32003);
        assert_eq!(
            explicit_gas.message(),
            "insufficient funds for gas * price + value: have 250000 want 400000"
        );

        let mut balance_during_call = request_with_input(
            addresses.limited,
            addresses.balance_reader,
            address_word(addresses.limited),
        );
        balance_during_call.gas = Some(200_000);
        balance_during_call.gas_price = Some(1);
        let balance_during_call = fixture
            .api
            .call(balance_during_call, anchor, None, None)
            .await
            .unwrap();
        assert_eq!(output_words(&balance_during_call), vec![U256::from(50_000)]);

        let persisted_balance = fixture
            .api
            .call(
                request_with_input(
                    addresses.observer,
                    addresses.balance_reader,
                    address_word(addresses.limited),
                ),
                anchor,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(output_words(&persisted_balance), vec![U256::from(250_000)]);

        let mut state_override = StateOverride::default();
        state_override.insert(
            addresses.environment,
            AccountOverride::default().with_code(Bytes::from_static(&[
                0x60, 0x2a, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
            ])),
        );
        let overridden = fixture
            .api
            .call(historical, anchor, Some(state_override), None)
            .await
            .unwrap();
        assert_eq!(output_words(&overridden), vec![U256::from(42)]);

        let code_hash_reader = Bytes::from_static(&[
            // return EXTCODEHASH(ADDRESS).
            0x30, 0x3f, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3,
        ]);
        let expected_code_hash = U256::from_be_slice(keccak256(&code_hash_reader).as_slice());
        let state_override = code_override(addresses.environment, code_hash_reader);
        let code_hash_request = call_request(addresses.funded, addresses.environment);
        let overridden = fixture
            .api
            .call(
                code_hash_request.clone(),
                anchor,
                Some(state_override.clone()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(output_words(&overridden), vec![expected_code_hash]);

        let contract = fixture
            .api
            .contract_multi_call_impl(
                vec![code_hash_request],
                anchor_context(),
                None,
                Some(state_override),
                Some(false),
                Some(false),
                Some(false),
            )
            .await
            .unwrap();
        assert_eq!(
            output_words(&contract.results[0].result),
            vec![expected_code_hash]
        );

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_call_returns_reth_compatible_execution_and_validation_errors() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let anchor = BlockId::Number(BlockNumberOrTag::Number(ANCHOR_NUMBER));
        let call = || call_request(addresses.funded, addresses.environment);

        let raw_revert = fixture
            .api
            .call(
                call(),
                anchor,
                Some(code_override(
                    addresses.environment,
                    hex::decode("63deadbeef6000526004601cfd").unwrap().into(),
                )),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(raw_revert.code(), 3);
        assert_eq!(raw_revert.message(), "execution reverted");
        assert_eq!(raw_revert.data().unwrap().get(), "\"0xdeadbeef\"");

        let reason_code =
            hex::decode("6308c379a060e01b5f5260206004526004602452636f6f707360e01b60445260645ffd")
                .unwrap();
        let reason_data =
            "0x08c379a00000000000000000000000000000000000000000000000000000000000000020\
             0000000000000000000000000000000000000000000000000000000000000004\
             6f6f707300000000000000000000000000000000000000000000000000000000";
        let decoded_revert = fixture
            .api
            .call(
                call(),
                anchor,
                Some(code_override(addresses.environment, reason_code.into())),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(decoded_revert.code(), 3);
        assert_eq!(decoded_revert.message(), "execution reverted: oops");
        assert_eq!(
            decoded_revert.data().unwrap().get(),
            format!("\"{}\"", reason_data.replace([' ', '\n'], ""))
        );

        let invalid_opcode = fixture
            .api
            .call(
                call(),
                anchor,
                Some(code_override(
                    addresses.environment,
                    Bytes::from_static(&[0xfe]),
                )),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(invalid_opcode.code(), -32003);
        assert_eq!(invalid_opcode.message(), "EVM error: InvalidFEOpcode");
        assert!(invalid_opcode.data().is_none());

        let out_of_gas = fixture
            .api
            .call(
                CallRequest {
                    inner: TransactionRequest::default()
                        .from(addresses.funded)
                        .to(addresses.environment)
                        .gas_limit(3_000_001),
                    tempo: None,
                },
                anchor,
                Some(code_override(
                    addresses.environment,
                    Bytes::from_static(&[0x5b, 0x60, 0x00, 0x56]),
                )),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(out_of_gas.code(), -32003);
        assert_eq!(
            out_of_gas.message(),
            "out of gas: gas required exceeds: 3000001"
        );
        assert!(out_of_gas.data().is_none());

        let intrinsic_gas = fixture
            .api
            .call(
                CallRequest {
                    inner: TransactionRequest::default()
                        .from(addresses.funded)
                        .to(addresses.environment)
                        .gas_limit(1),
                    tempo: None,
                },
                anchor,
                Some(code_override(
                    addresses.environment,
                    Bytes::from_static(&[0x00]),
                )),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(intrinsic_gas.code(), -32000);
        assert_eq!(intrinsic_gas.message(), "intrinsic gas too low");
        assert!(intrinsic_gas.data().is_none());

        let mut wrong_chain_request = TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.empty);
        wrong_chain_request.chain_id = Some(1);
        let wrong_chain = CallRequest {
            inner: wrong_chain_request,
            tempo: None,
        };
        let wrong_chain = fixture
            .api
            .call(wrong_chain, anchor, None, None)
            .await
            .unwrap_err();
        assert_eq!(wrong_chain.code(), -32000);
        assert_eq!(wrong_chain.message(), "invalid chain ID");

        let priority_above_fee = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.empty)
                .max_fee_per_gas(100_000_000_000)
                .max_priority_fee_per_gas(100_000_000_001),
            tempo: None,
        };
        let priority_above_fee = fixture
            .api
            .call(priority_above_fee, anchor, None, None)
            .await
            .unwrap_err();
        assert_eq!(priority_above_fee.code(), -32003);
        assert_eq!(
            priority_above_fee.message(),
            "max priority fee per gas higher than max fee per gas"
        );

        let fee_cap_too_low = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.empty)
                .max_fee_per_gas((ANCHOR_BASE_FEE - 1) as u128),
            tempo: None,
        };
        let fee_cap_too_low = fixture
            .api
            .call(fee_cap_too_low, anchor, None, None)
            .await
            .unwrap_err();
        assert_eq!(fee_cap_too_low.code(), -32000);
        assert_eq!(
            fee_cap_too_low.message(),
            "max fee per gas less than block base fee"
        );

        let mut conflicting_fees = TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.empty);
        conflicting_fees.gas_price = Some(ANCHOR_BASE_FEE as u128);
        conflicting_fees.max_fee_per_gas = Some(ANCHOR_BASE_FEE as u128);
        let conflicting_fees = fixture
            .api
            .call(
                CallRequest {
                    inner: conflicting_fees,
                    tempo: None,
                },
                anchor,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            conflicting_fees.code(),
            jsonrpsee::types::error::INVALID_PARAMS_CODE
        );
        assert_eq!(
            conflicting_fees.message(),
            "both gasPrice and (maxFeePerGas or maxPriorityFeePerGas) specified"
        );

        let mut conflicting_input = TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.empty);
        conflicting_input.input = TransactionInput {
            input: Some(Bytes::from_static(&[0x01])),
            data: Some(Bytes::from_static(&[0x02])),
        };
        let conflicting_input = fixture
            .api
            .call(
                CallRequest {
                    inner: conflicting_input,
                    tempo: None,
                },
                anchor,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            conflicting_input.code(),
            jsonrpsee::types::error::INVALID_PARAMS_CODE
        );
        assert_eq!(
            conflicting_input.message(),
            "both \"data\" and \"input\" are set and not equal. Please use \"input\" to pass transaction call data"
        );

        let mut empty_blobs = TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.empty);
        empty_blobs.blob_versioned_hashes = Some(Vec::new());
        let empty_blobs = fixture
            .api
            .call(
                CallRequest {
                    inner: empty_blobs,
                    tempo: None,
                },
                anchor,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(empty_blobs.code(), -32003);
        assert_eq!(
            empty_blobs.message(),
            "blob transaction missing blob hashes"
        );

        let blocked = fixture
            .api
            .call(
                call_request(addresses.blocked, addresses.empty),
                anchor,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(blocked.code(), -32603);
        assert!(blocked.message().contains("Blocked address"));
        assert!(blocked.data().is_none());

        let invalid_override = fixture
            .api
            .call(
                call(),
                anchor,
                Some(code_override(
                    addresses.environment,
                    Bytes::from_static(&[0xef, 0x01]),
                )),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(
            invalid_override.code(),
            jsonrpsee::types::error::INVALID_PARAMS_CODE
        );
        assert!(invalid_override.message().starts_with("Invalid bytecode: "));

        let insufficient = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.limited)
                .to(addresses.empty)
                .value(U256::from(250_001))
                .gas_price(1),
            tempo: None,
        };
        let eth_call_error = fixture
            .api
            .call(insufficient.clone(), anchor, None, None)
            .await
            .unwrap_err();
        assert_eq!(eth_call_error.code(), -32003);
        assert_eq!(
            eth_call_error.message(),
            "insufficient funds for gas * price + value: have 250000 want 250001"
        );
        assert!(eth_call_error.data().is_none());

        let eth_multi_error = fixture
            .api
            .multi_call(vec![insufficient.clone()], anchor, Some(false), None, None)
            .await
            .unwrap_err();
        assert_eq!(
            eth_multi_error.code(),
            DebankErrorCode::BalanceExhausted as i32
        );
        assert_eq!(eth_multi_error.message(), "Insufficient funds");

        let contract_multi_error = fixture
            .api
            .contract_multi_call_impl(
                vec![insufficient],
                anchor_context(),
                None,
                None,
                Some(false),
                Some(false),
                Some(false),
            )
            .await
            .unwrap_err();
        assert_eq!(
            contract_multi_error.code(),
            DebankErrorCode::BalanceExhausted as i32
        );
        assert_eq!(contract_multi_error.message(), "Insufficient funds");

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_multicalls_isolate_subcalls_and_preserve_each_fast_fail_contract() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let requests = vec![
            call_request(addresses.funded, addresses.counter),
            call_request(addresses.funded, addresses.counter),
            call_request(addresses.funded, addresses.revert),
            call_request(addresses.funded, addresses.counter),
        ];
        let anchor = BlockId::Number(BlockNumberOrTag::Number(ANCHOR_NUMBER));

        let eth = fixture
            .api
            .multi_call(
                requests.clone(),
                anchor,
                Some(true),
                Some(false),
                Some(false),
            )
            .await
            .unwrap();
        assert_eq!(output_words(&eth.results[0].result), vec![U256::ONE]);
        assert_eq!(output_words(&eth.results[1].result), vec![U256::ONE]);
        assert_eq!(
            eth.results[2].code,
            leafage_evm_types::MultiCallErrorCode::EVMReverted as i32
        );
        assert_eq!(
            eth.results[3].code,
            leafage_evm_types::MultiCallErrorCode::EVMFastFailed as i32
        );
        assert_eq!(eth.results[3].err, eth.results[2].err);
        assert!(!eth.stats.success);

        let contract = fixture
            .api
            .contract_multi_call_impl(
                requests,
                anchor_context(),
                None,
                None,
                Some(true),
                Some(false),
                Some(false),
            )
            .await
            .unwrap();
        assert_eq!(output_words(&contract.results[0].result), vec![U256::ONE]);
        assert_eq!(output_words(&contract.results[1].result), vec![U256::ONE]);
        assert_eq!(contract.results[2].code, DebankErrorCode::EvmRevert as i32);
        assert_eq!(
            serde_json::to_value(&contract.results[3]).unwrap(),
            serde_json::to_value(&contract.results[2]).unwrap()
        );
        assert!(!contract.stats.success);

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_call_like_gas_uses_allowance_block_limit_and_rpc_cap() {
        const RPC_GAS_CAP: u64 = 40_000_000;
        let fixture = build_arc_fixture_with_rpc_gas_cap(100, RPC_GAS_CAP);
        let addresses = fixture.addresses;
        let anchor = BlockId::Number(BlockNumberOrTag::Number(ANCHOR_NUMBER));
        let requests = || {
            vec![
                // Missing gas is bounded by the caller's 250k allowance.
                CallRequest {
                    inner: TransactionRequest::default()
                        .from(addresses.limited)
                        .to(addresses.gas_guard)
                        .gas_price(1),
                    tempo: None,
                },
                // The 40m RPC cap exceeds the 30m block limit, so missing gas
                // is still bounded by the block.
                CallRequest {
                    inner: TransactionRequest::default()
                        .from(addresses.funded)
                        .to(addresses.block_gas_guard)
                        .gas_price(1),
                    tempo: None,
                },
                // An explicit limit is not bounded by the block gas limit.
                CallRequest {
                    inner: TransactionRequest::default()
                        .from(addresses.funded)
                        .to(addresses.block_gas_guard)
                        .gas_limit(RPC_GAS_CAP)
                        .gas_price(1),
                    tempo: None,
                },
                // Explicit gas above a nonzero RPC cap is truncated to 40m,
                // which remains below this guard's 45m threshold.
                CallRequest {
                    inner: TransactionRequest::default()
                        .from(addresses.funded)
                        .to(addresses.rpc_cap_guard)
                        .gas_limit(50_000_000)
                        .gas_price(1),
                    tempo: None,
                },
            ]
        };

        let eth_requests = requests();
        assert!(fixture
            .api
            .call(eth_requests[0].clone(), anchor, None, None)
            .await
            .is_err());
        assert!(fixture
            .api
            .call(eth_requests[1].clone(), anchor, None, None)
            .await
            .is_err());
        assert_eq!(
            fixture
                .api
                .call(eth_requests[2].clone(), anchor, None, None)
                .await
                .unwrap(),
            Bytes::new()
        );
        assert!(fixture
            .api
            .call(eth_requests[3].clone(), anchor, None, None)
            .await
            .is_err());

        let eth_multi = fixture
            .api
            .multi_call(requests(), anchor, Some(false), Some(false), Some(false))
            .await
            .unwrap();
        assert_eq!(
            eth_multi
                .results
                .iter()
                .map(|result| result.code)
                .collect::<Vec<_>>(),
            vec![
                leafage_evm_types::MultiCallErrorCode::EVMReverted as i32,
                leafage_evm_types::MultiCallErrorCode::EVMReverted as i32,
                leafage_evm_types::MultiCallErrorCode::Success as i32,
                leafage_evm_types::MultiCallErrorCode::EVMReverted as i32,
            ]
        );

        let contract_multi = fixture
            .api
            .contract_multi_call_impl(
                requests(),
                anchor_context(),
                None,
                None,
                Some(false),
                Some(false),
                Some(false),
            )
            .await
            .unwrap();
        assert_eq!(
            contract_multi
                .results
                .iter()
                .map(|result| result.code)
                .collect::<Vec<_>>(),
            vec![
                DebankErrorCode::EvmRevert as i32,
                DebankErrorCode::EvmRevert as i32,
                0,
                DebankErrorCode::EvmRevert as i32,
            ]
        );

        let mut during = request_with_input(
            addresses.limited,
            addresses.balance_reader,
            address_word(addresses.limited),
        );
        during.gas = Some(200_000);
        during.gas_price = Some(1);
        let persisted = request_with_input(
            addresses.observer,
            addresses.balance_reader,
            address_word(addresses.limited),
        );

        let eth_balances = fixture
            .api
            .multi_call(
                vec![during.clone(), persisted.clone()],
                anchor,
                Some(false),
                Some(false),
                Some(false),
            )
            .await
            .unwrap();
        assert_eq!(
            output_words(&eth_balances.results[0].result),
            vec![U256::from(50_000)]
        );
        assert_eq!(
            output_words(&eth_balances.results[1].result),
            vec![U256::from(250_000)]
        );

        let contract_balances = fixture
            .api
            .contract_multi_call_impl(
                vec![during, persisted],
                anchor_context(),
                None,
                None,
                Some(false),
                Some(false),
                Some(false),
            )
            .await
            .unwrap();
        assert_eq!(
            output_words(&contract_balances.results[0].result),
            vec![U256::from(50_000)]
        );
        assert_eq!(
            output_words(&contract_balances.results[1].result),
            vec![U256::from(250_000)]
        );

        let explicit_insufficient = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.limited)
                .to(addresses.empty)
                .gas_limit(400_000)
                .gas_price(1),
            tempo: None,
        };
        let eth_error = fixture
            .api
            .multi_call(
                vec![explicit_insufficient.clone()],
                anchor,
                Some(false),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(eth_error.code(), DebankErrorCode::BalanceExhausted as i32);
        let contract_error = fixture
            .api
            .contract_multi_call_impl(
                vec![explicit_insufficient],
                anchor_context(),
                None,
                None,
                Some(false),
                Some(false),
                Some(false),
            )
            .await
            .unwrap_err();
        assert_eq!(
            contract_error.code(),
            DebankErrorCode::BalanceExhausted as i32
        );

        let effective_fee_request = || CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.fee_limited)
                .to(addresses.gas_guard)
                .max_fee_per_gas(1_000)
                .max_priority_fee_per_gas(1),
            tempo: None,
        };
        assert_eq!(
            fixture
                .api
                .call(effective_fee_request(), anchor, None, None)
                .await
                .unwrap(),
            Bytes::new()
        );
        let eth = fixture
            .api
            .multi_call(
                vec![effective_fee_request()],
                anchor,
                Some(false),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(eth.results[0].code, 0);
        let contract = fixture
            .api
            .contract_multi_call_impl(
                vec![effective_fee_request()],
                anchor_context(),
                None,
                None,
                Some(false),
                Some(false),
                Some(false),
            )
            .await
            .unwrap();
        assert_eq!(contract.results[0].code, 0);

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_simulation_commits_sequential_state_fees_and_exact_fast_stop() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let initial_funded_balance = U256::ONE << 128;
        let transferred = U256::from(123);
        let first = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.counter)
                .value(transferred)
                .gas_limit(500_000)
                // Raw simulation semantics fill maxFee=max(basefee, tip)=5.
                // The call-like converter would instead use basefee+tip=8.
                .max_priority_fee_per_gas(5),
            tempo: None,
        };
        let second = call_request(addresses.funded, addresses.counter);
        let counter_balance = request_with_input(
            addresses.observer,
            addresses.balance_reader,
            address_word(addresses.counter),
        );
        let funded_balance = request_with_input(
            addresses.observer,
            addresses.balance_reader,
            address_word(addresses.funded),
        );
        let beneficiary_balance = request_with_input(
            addresses.observer,
            addresses.balance_reader,
            address_word(addresses.beneficiary),
        );
        let simulated = fixture
            .api
            .simulate_transactions(
                vec![
                    first,
                    second,
                    counter_balance,
                    funded_balance,
                    beneficiary_balance,
                ],
                anchor_context(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(simulated.stats.block_num, ANCHOR_NUMBER);
        assert_eq!(simulated.stats.block_hash, H256::repeat_byte(0xbb));
        assert!(simulated.stats.success);
        assert_eq!(
            output_words(&root_trace_output(&simulated.results[0])),
            vec![U256::ONE]
        );
        assert_eq!(
            output_words(&root_trace_output(&simulated.results[1])),
            vec![U256::from(2)]
        );
        assert_eq!(
            output_words(&root_trace_output(&simulated.results[2])),
            vec![transferred]
        );
        let first_fee = U256::from(simulated.results[0].gas_used) * U256::from(5);
        assert_eq!(
            output_words(&root_trace_output(&simulated.results[3])),
            vec![initial_funded_balance - transferred - first_fee]
        );
        assert_eq!(
            output_words(&root_trace_output(&simulated.results[4])),
            vec![first_fee]
        );

        let stopped = fixture
            .api
            .simulate_transactions(
                vec![
                    call_request(addresses.funded, addresses.revert),
                    call_request(addresses.funded, addresses.counter),
                ],
                anchor_context(),
                None,
            )
            .await
            .unwrap();
        assert!(!stopped.stats.success);
        assert_eq!(stopped.results[0].code, DebankErrorCode::EvmRevert as i32);
        assert_eq!(
            serde_json::to_value(&stopped.results[1]).unwrap(),
            serde_json::to_value(&stopped.results[0]).unwrap()
        );

        let explicit_nonce = vec![
            CallRequest {
                inner: TransactionRequest::default()
                    .from(addresses.funded)
                    .to(addresses.counter)
                    .nonce(0),
                tempo: None,
            },
            CallRequest {
                inner: TransactionRequest::default()
                    .from(addresses.funded)
                    .to(addresses.counter)
                    .nonce(0),
                tempo: None,
            },
        ];
        let error = fixture
            .api
            .simulate_transactions(explicit_nonce, anchor_context(), None)
            .await
            .unwrap_err();
        assert_eq!(error.code(), DebankErrorCode::NonceError as i32);

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_simulation_commits_eip7702_delegation_for_the_next_transaction() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let signer: PrivateKeySigner =
            "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412d9d780c2350c7d"
                .parse()
                .unwrap();
        let authority = signer.address();
        let authorization = Authorization {
            chain_id: U256::from(ARC_MAINNET_CHAIN_ID),
            address: addresses.counter,
            nonce: 0,
        };
        let signature = signer
            .sign_hash_sync(&authorization.signature_hash())
            .unwrap();
        let mut authorize = TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.empty)
            .gas_limit(500_000);
        authorize.authorization_list = Some(vec![authorization.into_signed(signature)]);

        let simulated = fixture
            .api
            .simulate_transactions(
                vec![
                    CallRequest {
                        inner: authorize,
                        tempo: None,
                    },
                    call_request(addresses.funded, authority),
                ],
                anchor_context(),
                None,
            )
            .await
            .unwrap();
        assert!(simulated.stats.success, "{simulated:#?}");
        assert_eq!(
            output_words(&root_trace_output(&simulated.results[1])),
            vec![U256::ONE]
        );

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_simulation_only_runs_eip2935_for_explicit_h_plus_one() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let anchor = BlockId::Number(BlockNumberOrTag::Number(ANCHOR_NUMBER));
        let request = request_with_input(
            addresses.funded,
            HISTORY_STORAGE_ADDRESS,
            U256::from(ANCHOR_NUMBER).to_be_bytes::<32>().into(),
        );
        let environment_request = call_request(addresses.funded, addresses.environment);
        let next = BlockOverrides::default().with_number(U256::from(ANCHOR_NUMBER + 1));

        let direct = fixture
            .api
            .call(request.clone(), anchor, None, Some(next.clone()))
            .await
            .unwrap();
        assert_eq!(output_words(&direct), vec![U256::ZERO]);

        let at_anchor = fixture
            .api
            .simulate_transactions(vec![request.clone()], anchor_context(), None)
            .await
            .unwrap();
        assert!(root_trace_output(&at_anchor.results[0]).is_empty());
        let at_anchor_environment = fixture
            .api
            .simulate_transactions(vec![environment_request.clone()], anchor_context(), None)
            .await
            .unwrap();
        let words = output_words(&root_trace_output(&at_anchor_environment.results[0]));
        assert_eq!(words[0], U256::from(ANCHOR_NUMBER));
        assert_eq!(words[1], U256::from(ANCHOR_BASE_FEE));

        let base_fee_override = BlockOverrides {
            base_fee: Some(U256::from(99)),
            ..Default::default()
        };
        let base_fee_only = fixture
            .api
            .simulate_transactions(
                vec![request.clone()],
                anchor_context(),
                Some(base_fee_override.clone()),
            )
            .await
            .unwrap();
        assert!(root_trace_output(&base_fee_only.results[0]).is_empty());
        let base_fee_environment = fixture
            .api
            .simulate_transactions(
                vec![environment_request.clone()],
                anchor_context(),
                Some(base_fee_override),
            )
            .await
            .unwrap();
        let words = output_words(&root_trace_output(&base_fee_environment.results[0]));
        assert_eq!(words[0], U256::from(ANCHOR_NUMBER));
        assert_eq!(words[1], U256::from(99));

        let simulated = fixture
            .api
            .simulate_transactions(
                vec![request.clone(), environment_request],
                anchor_context(),
                Some(next),
            )
            .await
            .unwrap();
        assert_eq!(simulated.stats.block_num, ANCHOR_NUMBER);
        assert_eq!(
            root_trace_output(&simulated.results[0]),
            Bytes::copy_from_slice(H256::repeat_byte(0xbb).as_slice())
        );
        let words = output_words(&root_trace_output(&simulated.results[1]));
        assert_eq!(words[0], U256::from(ANCHOR_NUMBER + 1));
        assert_eq!(words[1], U256::from(NEXT_BASE_FEE));
        assert_eq!(
            words[3],
            U256::from_be_slice(H256::repeat_byte(0xbb).as_slice())
        );

        let wrong = BlockOverrides::default().with_number(U256::from(ANCHOR_NUMBER + 2));
        let error = fixture
            .api
            .simulate_transactions(vec![request], anchor_context(), Some(wrong))
            .await
            .unwrap_err();
        assert_eq!(
            error.message(),
            "simulation block number must equal state anchor + 1"
        );

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_simulation_preserves_system_normal_and_delegatecall_log_emitters() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let value_log = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.logger)
                .value(U256::from(5)),
            tempo: None,
        };
        let result = fixture
            .api
            .simulate_transactions(
                vec![
                    value_log,
                    call_request(addresses.funded, addresses.delegate_proxy),
                ],
                anchor_context(),
                None,
            )
            .await
            .unwrap();
        assert!(result.stats.success);
        assert_eq!(
            result.results[0].events.len(),
            2,
            "events: {:#?}",
            result.results[0].events
        );
        assert_eq!(
            result.results[0].events[0].contract_id,
            "0xfffffffffffffffffffffffffffffffffffffffe"
                .parse::<Address>()
                .unwrap()
        );
        assert_eq!(result.results[0].events[1].contract_id, addresses.logger);
        assert_eq!(result.results[1].events.len(), 1);
        assert_eq!(
            result.results[1].events[0].contract_id,
            addresses.delegate_proxy
        );
        assert_ne!(
            result.results[1].events[0].contract_id,
            addresses.delegate_implementation
        );

        let reverted = fixture
            .api
            .simulate_transactions(
                vec![CallRequest {
                    inner: TransactionRequest::default()
                        .from(addresses.funded)
                        .to(addresses.logger_revert)
                        .value(U256::from(5)),
                    tempo: None,
                }],
                anchor_context(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(reverted.results[0].code, DebankErrorCode::EvmRevert as i32);
        assert!(reverted.results[0].events.is_empty());

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_simulation_preserves_selfdestruct_event_and_suicide_child() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let nonzero = fixture
            .api
            .simulate_transactions(
                vec![call_request(
                    addresses.funded,
                    addresses.selfdestruct_nonzero,
                )],
                anchor_context(),
                None,
            )
            .await
            .unwrap();
        let zero = fixture
            .api
            .simulate_transactions(
                vec![call_request(addresses.funded, addresses.selfdestruct_zero)],
                anchor_context(),
                None,
            )
            .await
            .unwrap();
        let reverted = fixture
            .api
            .simulate_transactions(
                vec![call_request(
                    addresses.funded,
                    addresses.selfdestruct_revert_parent,
                )],
                anchor_context(),
                None,
            )
            .await
            .unwrap();

        let nonzero_result = &nonzero.results[0];
        let zero_result = &zero.results[0];
        let reverted_result = &reverted.results[0];
        assert_eq!(nonzero_result.code, 0);
        assert_eq!(nonzero_result.traces.len(), 2);
        let root = &nonzero_result.traces[0];
        let suicide = &nonzero_result.traces[1];
        assert_eq!(root.call_create_type, "call");
        assert_eq!(root.call_type, "CALL");
        assert!(root.parent_trace_id.is_empty());
        assert_eq!(root.pos_in_parent_trace, 0);
        assert_eq!(root.id, root.debank_id());
        assert_eq!(suicide.call_create_type, "suicide");
        assert!(suicide.call_type.is_empty());
        assert_eq!(suicide.parent_trace_id, root.id);
        assert_eq!(suicide.pos_in_parent_trace, 1);
        assert_eq!(suicide.from_addr, addresses.selfdestruct_nonzero);
        assert_eq!(suicide.to_addr, addresses.selfdestruct_beneficiary);
        assert_eq!(suicide.value, U256::from(42));
        assert_eq!(suicide.id, suicide.debank_id());
        assert_eq!(nonzero_result.events.len(), 1);
        assert_arc_transfer_event(
            &nonzero_result.events[0],
            addresses.selfdestruct_nonzero,
            addresses.selfdestruct_beneficiary,
            U256::from(42),
            &root.id,
            0,
        );

        assert_eq!(zero_result.code, 0);
        assert!(zero_result.events.is_empty());
        assert_eq!(zero_result.traces.len(), 2);
        let zero_root = &zero_result.traces[0];
        let zero_suicide = &zero_result.traces[1];
        assert_eq!(zero_root.call_create_type, "call");
        assert_eq!(zero_root.call_type, "CALL");
        assert_eq!(zero_suicide.call_create_type, "suicide");
        assert_eq!(zero_suicide.parent_trace_id, zero_root.id);
        assert_eq!(zero_suicide.pos_in_parent_trace, 0);
        assert_eq!(zero_suicide.from_addr, addresses.selfdestruct_zero);
        assert_eq!(zero_suicide.to_addr, addresses.selfdestruct_beneficiary);
        assert_eq!(zero_suicide.value, U256::ZERO);
        assert_eq!(zero_suicide.id, zero_suicide.debank_id());

        assert_eq!(reverted_result.code, DebankErrorCode::EvmRevert as i32);
        assert!(
            reverted_result.events.is_empty(),
            "parent REVERT leaked SELFDESTRUCT event: {reverted_result:#?}"
        );

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_inspector_resets_log_cursor_after_reverted_child() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let simulated = fixture
            .api
            .simulate_transactions(
                vec![call_request(
                    addresses.funded,
                    addresses.reverted_child_then_selfdestruct,
                )],
                anchor_context(),
                None,
            )
            .await
            .unwrap();
        let result = &simulated.results[0];

        assert_eq!(result.code, 0);
        // The reverted child and both of its logs are omitted. The parent
        // remains a CALL root with its later SELFDESTRUCT action appended.
        assert_eq!(result.traces.len(), 2, "{result:#?}");
        let root = &result.traces[0];
        let suicide = &result.traces[1];
        assert_eq!(root.call_create_type, "call");
        assert_eq!(root.call_type, "CALL");
        assert_eq!(suicide.call_create_type, "suicide");
        assert_eq!(suicide.parent_trace_id, root.id);
        assert_eq!(suicide.pos_in_parent_trace, 1);
        assert_eq!(
            suicide.from_addr,
            addresses.reverted_child_then_selfdestruct
        );
        assert_eq!(suicide.to_addr, addresses.selfdestruct_beneficiary);
        assert_eq!(suicide.value, U256::from(10));
        assert_eq!(result.events.len(), 1, "{result:#?}");
        assert_arc_transfer_event(
            &result.events[0],
            addresses.reverted_child_then_selfdestruct,
            addresses.selfdestruct_beneficiary,
            U256::from(10),
            &root.id,
            0,
        );

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_simulation_preserves_create_value_transfer_event_fields() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let value = U256::from(9);
        let created = addresses.funded.create(0);
        let runtime = counter_code();
        let request = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .value(value)
                .input(TransactionInput::new(init_code(&runtime))),
            tempo: None,
        };
        let simulated = fixture
            .api
            .simulate_transactions(
                vec![request, call_request(addresses.funded, created)],
                anchor_context(),
                None,
            )
            .await
            .unwrap();
        let result = &simulated.results[0];

        assert_eq!(result.code, 0);
        assert_eq!(result.traces.len(), 1);
        let root = &result.traces[0];
        assert_eq!(root.call_create_type, "create");
        assert_eq!(root.to_addr, created);
        assert_eq!(root.value, value);
        assert_eq!(result.events.len(), 1);
        assert_arc_transfer_event(
            &result.events[0],
            addresses.funded,
            created,
            value,
            &root.id,
            0,
        );
        assert_eq!(
            output_words(&root_trace_output(&simulated.results[1])),
            vec![U256::ONE]
        );

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_inspector_orders_value_and_precompile_logs_and_rolls_both_back_on_oog() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let native_coin_authority: Address = "0x1800000000000000000000000000000000000000"
            .parse()
            .unwrap();
        let value = U256::from(7);
        let minted = U256::from(10);
        let input = native_coin_mint_input(addresses.empty, minted);
        let request = |gas_limit| CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.native_fiat_token)
                .to(native_coin_authority)
                .value(value)
                .gas_limit(gas_limit)
                .input(TransactionInput::new(input.clone())),
            tempo: None,
        };

        let successful = fixture
            .api
            .simulate_transactions(vec![request(100_000)], anchor_context(), None)
            .await
            .unwrap();
        assert!(successful.stats.success, "{successful:#?}");
        let success = &successful.results[0];
        assert_eq!(success.code, 0);
        assert_eq!(success.events.len(), 2, "{success:#?}");
        let root = &success.traces[0];
        assert_arc_transfer_event(
            &success.events[0],
            addresses.native_fiat_token,
            native_coin_authority,
            value,
            &root.id,
            0,
        );
        assert_arc_transfer_event(
            &success.events[1],
            Address::ZERO,
            addresses.empty,
            minted,
            &root.id,
            1,
        );

        let failed = fixture
            .api
            .simulate_transactions(vec![request(success.gas_used - 1)], anchor_context(), None)
            .await
            .unwrap();
        assert!(!failed.stats.success, "{failed:#?}");
        assert_eq!(
            failed.results[0].code,
            DebankErrorCode::GasExhausted as i32,
            "{failed:#?}"
        );
        assert!(failed.results[0].events.is_empty(), "{failed:#?}");

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_simulation_keeps_nested_zero_value_precompile_trace_and_event() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let native_coin_control: Address = "0x1800000000000000000000000000000000000001"
            .parse()
            .unwrap();

        let simulated = fixture
            .api
            .simulate_transactions(
                vec![call_request(addresses.funded, addresses.native_fiat_token)],
                anchor_context(),
                None,
            )
            .await
            .unwrap();

        assert!(simulated.stats.success, "{simulated:#?}");
        let result = &simulated.results[0];
        assert_eq!(result.code, 0, "{result:#?}");
        assert_eq!(output_words(&root_trace_output(result)), vec![U256::ONE]);
        assert_eq!(result.traces.len(), 2, "{result:#?}");

        let root = &result.traces[0];
        let precompile = &result.traces[1];
        assert_eq!(precompile.to_addr, native_coin_control);
        assert_eq!(precompile.value, U256::ZERO);
        assert_eq!(precompile.parent_trace_id, root.id);
        assert_eq!(precompile.pos_in_parent_trace, 0);

        assert_eq!(result.events.len(), 1, "{result:#?}");
        let event = &result.events[0];
        assert_eq!(event.contract_id, native_coin_control);
        assert_eq!(
            event.selector,
            keccak256("Blocklisted(address)").to_string()
        );
        assert_eq!(
            event.topics,
            vec![H256::left_padding_from(addresses.empty.as_slice()).to_string()]
        );
        assert!(event.data.is_empty());
        assert_eq!(event.parent_trace_id, precompile.id);
        assert_eq!(event.pos_in_parent_trace, 0);
        assert_eq!(event.id, event.debank_id());

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_simulation_returns_rpc_error_for_top_level_invalid_transaction() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let error = fixture
            .api
            .simulate_transactions(
                vec![call_request(addresses.blocked, addresses.empty)],
                anchor_context(),
                None,
            )
            .await
            .unwrap_err();

        assert_eq!(error.code(), DebankErrorCode::EvmFailed as i32);
        assert!(error.message().contains("Blocked address"));

        fixture.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn arc_normal_and_inspect_paths_share_nca_pq_and_p256_precompiles() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;
        let anchor = BlockId::Number(BlockNumberOrTag::Number(ANCHOR_NUMBER));
        let nca: Address = "0x1800000000000000000000000000000000000000"
            .parse()
            .unwrap();
        let pq: Address = "0x1800000000000000000000000000000000000004"
            .parse()
            .unwrap();
        let p256: Address = "0x0000000000000000000000000000000000000100"
            .parse()
            .unwrap();
        let nca_request = request_with_input(addresses.funded, nca, selector("totalSupply()"));
        let p256_request = request_with_input(addresses.funded, p256, p256_valid_input());

        let nca_call = fixture
            .api
            .call(nca_request.clone(), anchor, None, None)
            .await
            .unwrap();
        let p256_call = fixture
            .api
            .call(p256_request.clone(), anchor, None, None)
            .await
            .unwrap();
        assert_eq!(output_words(&nca_call), vec![U256::ZERO]);
        assert_eq!(output_words(&p256_call), vec![U256::ONE]);

        let inspected = fixture
            .api
            .simulate_transactions(vec![nca_request, p256_request], anchor_context(), None)
            .await
            .unwrap();
        assert_eq!(root_trace_output(&inspected.results[0]), nca_call);
        assert_eq!(root_trace_output(&inspected.results[1]), p256_call);

        let malformed_pq = request_with_input(
            addresses.funded,
            pq,
            selector("verifySlhDsaSha2128s(bytes,bytes,bytes)"),
        );
        assert!(fixture
            .api
            .call(malformed_pq.clone(), anchor, None, None)
            .await
            .is_err());
        let inspected_pq = fixture
            .api
            .simulate_transactions(vec![malformed_pq], anchor_context(), None)
            .await
            .unwrap();
        assert_eq!(
            inspected_pq.results[0].code,
            DebankErrorCode::EvmRevert as i32
        );

        fixture.close();
    }

    #[test]
    fn zero_rpc_gas_cap_is_unlimited_but_consensus_and_block_caps_still_apply() {
        assert_eq!(
            estimate_gas_limit_cap(Some(0), u64::MAX, 30_000_000),
            30_000_000
        );
        assert_eq!(
            estimate_gas_limit_cap(Some(0), 16_777_216, 30_000_000),
            16_777_216
        );
        assert_eq!(
            estimate_gas_limit_cap(Some(25_000_000), u64::MAX, 30_000_000),
            25_000_000
        );
    }

    #[test]
    fn arc_estimate_handles_transfer_value_and_fee_errors() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;

        let transfer = call_request(addresses.funded, addresses.empty);
        assert_eq!(
            estimate(&fixture.api, transfer, None).unwrap(),
            U256::from(MIN_TRANSACTION_GAS)
        );

        let transfer_with_low_request_gas = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.empty)
                .gas_limit(20_000),
            tempo: None,
        };
        assert_eq!(
            estimate(&fixture.api, transfer_with_low_request_gas, None).unwrap(),
            U256::from(MIN_TRANSACTION_GAS)
        );

        let value_without_balance = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.empty)
                .to(addresses.funded)
                .value(U256::ONE),
            tempo: None,
        };
        let error = estimate(&fixture.api, value_without_balance, None).unwrap_err();
        assert_eq!(error.code(), DebankErrorCode::BalanceExhausted as i32);
        assert_eq!(error.message(), "Insufficient funds");

        let fee_without_balance = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.empty)
                .to(addresses.funded)
                .gas_price(1),
            tempo: None,
        };
        let error = estimate(&fixture.api, fee_without_balance, None).unwrap_err();
        assert_eq!(error.code(), DebankErrorCode::GasExhausted as i32);
        assert_eq!(error.message(), "Invalid gas limit");

        let large_balance_allowance = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.environment)
                .gas_price(1),
            tempo: None,
        };
        assert!(
            estimate(&fixture.api, large_balance_allowance, None).unwrap()
                > U256::from(MIN_TRANSACTION_GAS)
        );

        let contract_sender = call_request(addresses.environment, addresses.empty);
        assert_eq!(
            estimate(&fixture.api, contract_sender, None).unwrap(),
            U256::from(MIN_TRANSACTION_GAS)
        );

        fixture.close();
    }

    #[test]
    fn arc_estimate_handles_gas_dependent_revert_and_returns_executable_gas() {
        let fixture = build_arc_fixture(100);
        let request = call_request(fixture.addresses.funded, fixture.addresses.gas_guard);

        let estimated: u64 = estimate(&fixture.api, request.clone(), None)
            .unwrap()
            .try_into()
            .unwrap();
        assert!(estimated < ARC_RPC_GAS_CAP);
        assert!(matches!(
            execute_arc_estimate_probe(&fixture.api, request.clone(), None, estimated),
            ExecutionResult::Success { .. }
        ));
        assert!(matches!(
            execute_arc_estimate_probe(
                &fixture.api,
                request,
                None,
                estimated.saturating_sub(20_000),
            ),
            ExecutionResult::Revert { .. }
        ));

        fixture.close();
    }

    #[test]
    fn arc_estimate_uses_generic_block_overrides() {
        let fixture = build_arc_fixture(100);
        let request = call_request(fixture.addresses.funded, fixture.addresses.environment);

        let h_gas: u64 = estimate(&fixture.api, request.clone(), None)
            .unwrap()
            .try_into()
            .unwrap();
        let h_output = success_output(execute_arc_estimate_probe(
            &fixture.api,
            request.clone(),
            None,
            h_gas,
        ));
        let h_words = output_words(&h_output);
        assert_eq!(h_words[0], U256::from(ANCHOR_NUMBER));
        assert_eq!(h_words[1], U256::from(ANCHOR_BASE_FEE));
        assert_eq!(
            h_words[2],
            U256::from_be_slice(H256::repeat_byte(0xaa).as_slice())
        );

        let overridden_hash = H256::repeat_byte(0xcc);
        let mut block_hashes = BTreeMap::new();
        block_hashes.insert(ANCHOR_NUMBER, overridden_hash);
        let overridden = BlockOverrides {
            number: Some(U256::from(ANCHOR_NUMBER + 1)),
            base_fee: Some(U256::from(11)),
            block_hash: Some(block_hashes),
            ..Default::default()
        };
        let overridden_gas: u64 = estimate(&fixture.api, request.clone(), Some(overridden.clone()))
            .unwrap()
            .try_into()
            .unwrap();
        let overridden_output = success_output(execute_arc_estimate_probe(
            &fixture.api,
            request,
            Some(overridden),
            overridden_gas,
        ));
        let overridden_words = output_words(&overridden_output);
        assert_eq!(overridden_words[0], U256::from(ANCHOR_NUMBER + 1));
        assert_eq!(overridden_words[1], U256::from(11));
        assert_eq!(
            overridden_words[3],
            U256::from_be_slice(overridden_hash.as_slice())
        );

        fixture.close();
    }

    #[test]
    fn arc_estimate_preserves_revert_and_normal_transaction_errors() {
        let fixture = build_arc_fixture(100);
        let addresses = fixture.addresses;

        let revert = estimate(
            &fixture.api,
            call_request(addresses.funded, addresses.revert),
            None,
        )
        .unwrap_err();
        assert_eq!(revert.code(), DebankErrorCode::EvmRevert as i32);
        assert_eq!(revert.message(), "");

        let blocked = estimate(
            &fixture.api,
            call_request(addresses.blocked, addresses.empty),
            None,
        )
        .unwrap_err();
        assert_eq!(blocked.code(), DebankErrorCode::EvmFailed as i32);
        assert!(blocked.message().contains("Blocked address"));

        let invalid_fee = CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.empty)
                .gas_price(1)
                .max_fee_per_gas(2),
            tempo: None,
        };
        let invalid_fee = estimate(&fixture.api, invalid_fee, None).unwrap_err();
        assert_eq!(invalid_fee.code(), DebankErrorCode::InvalidParams as i32);
        assert_eq!(invalid_fee.message(), "Invalid fee parameters");

        let mut empty_authorization = TransactionRequest::default()
            .from(addresses.funded)
            .to(addresses.empty);
        empty_authorization.authorization_list = Some(Vec::new());
        let empty_authorization = estimate(
            &fixture.api,
            CallRequest {
                inner: empty_authorization,
                tempo: None,
            },
            None,
        )
        .unwrap_err();
        assert_eq!(
            empty_authorization.code(),
            DebankErrorCode::EvmFailed as i32
        );
        assert!(empty_authorization
            .message()
            .to_ascii_lowercase()
            .contains("authorization"));

        fixture.close();
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

/// Shared message shape for "local failed, historical also failed".
/// blockx_stateReadBatch item errors reuse it so batch and single
/// requests stay byte-identical on this path.
#[inline]
pub(crate) fn combine_error_message(
    local_message: &str,
    historical_err: &jsonrpsee::core::ClientError,
) -> String {
    format!(
        "Local error: {}; Historical RPC error: {}",
        local_message, historical_err
    )
}

#[inline]
fn combine_errors(
    local_err: jsonrpsee::types::ErrorObjectOwned,
    historical_err: jsonrpsee::core::ClientError,
) -> jsonrpsee::types::ErrorObjectOwned {
    rpc_error_with_code(
        local_err.code(),
        combine_error_message(local_err.message(), &historical_err),
    )
}

#[inline]
fn map_historical_error(
    local_err: Option<jsonrpsee::types::ErrorObjectOwned>,
    historical_err: jsonrpsee::core::ClientError,
) -> jsonrpsee::types::ErrorObjectOwned {
    if is_historical_rpc_overloaded(&historical_err) {
        return historical_rpc_overloaded_error();
    }

    match local_err {
        Some(local_err) => combine_errors(local_err, historical_err),
        None => rpc_error_with_code(
            DebankErrorCode::InternalError as i32,
            format!("Historical RPC error: {}", historical_err),
        ),
    }
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
        match self
            .debank_get_address_nonce_impl(address, block_ctx.clone())
            .await
        {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.should_try_historical(&block_ctx) {
                    match historical_client
                        .get_address_nonce(address, block_ctx)
                        .await
                    {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(map_historical_error(Some(err), historical_err)),
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
        match self
            .debank_get_address_balance_impl(address, block_ctx.clone())
            .await
        {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.should_try_historical(&block_ctx) {
                    match historical_client
                        .get_address_balance(address, block_ctx)
                        .await
                    {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(map_historical_error(Some(err), historical_err)),
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
        match self.debank_get_code_impl(address, block_ctx.clone()).await {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.should_try_historical(&block_ctx) {
                    match historical_client.get_address_code(address, block_ctx).await {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(map_historical_error(Some(err), historical_err)),
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
        match self
            .debank_get_storage_at_impl(address, position.as_b256(), block_ctx.clone())
            .await
        {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.should_try_historical(&block_ctx) {
                    match historical_client
                        .get_storage_at(address, position, block_ctx)
                        .await
                    {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(map_historical_error(Some(err), historical_err)),
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
                        Err(historical_err) => Err(map_historical_error(Some(err), historical_err)),
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
                        Err(historical_err) => Err(map_historical_error(Some(err), historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn get_latest_block(&self) -> RpcResult<DebankBlock> {
        self.debank_get_latest_block_impl().await
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
                    .map_err(|error| map_historical_error(None, error));
            }
        }

        self.debank_get_block_by_height_impl(height).await
    }

    async fn get_block_by_id(&self, id: H256) -> RpcResult<DebankBlock> {
        match self.debank_get_block_by_id_impl(id).await {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.inner.historical_client() {
                    match historical_client.get_block_by_id(id).await {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(map_historical_error(Some(err), historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn block_is_valid(&self, id: H256) -> RpcResult<bool> {
        match self.block_is_valid_impl(id).await {
            Ok(result) => Ok(result),
            Err(err) => {
                if let Some(historical_client) = self.inner.historical_client() {
                    match historical_client.block_is_valid(id).await {
                        Ok(result) => Ok(result),
                        Err(historical_err) => Err(map_historical_error(Some(err), historical_err)),
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
                        Err(historical_err) => Err(map_historical_error(Some(err), historical_err)),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }
}
