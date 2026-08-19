use super::utils;
use crate::api::{DebankApiClient, DebankApiServer};
use crate::api_impl::core::{
    Api, ApiCore, EstimateGasPolicy, EvmExecutor, GetHaltReason, GetTransactionError,
    ToJsonRpcError, TxSetter,
};
use crate::api_impl::historical_overload::{
    historical_rpc_overloaded_error, is_historical_rpc_overloaded,
};
use crate::api_impl::utils::build_debank_traces;
use crate::error::{internal_rpc_err, invalid_params_rpc_err, rpc_error_with_code};

use alloy::rpc::types::state::StateOverride;
use alloy::sol_types::{decode_revert_reason, SolValue};
use jsonrpsee::{core::RpcResult, http_client::HttpClient};
use leafage_evm_chains::arc::{build_arc_query_environment, ArcQueryKind};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EstimateGasLimits {
    /// Block/RPC/protocol cap. Arc uses this for EVM validation and for the
    /// one error-classification retry that may exceed an explicit request cap.
    execution_hard_cap: u64,
    /// Request-local search and return cap.
    search_cap: u64,
}

impl EstimateGasPolicy {
    fn limits(self, request_gas_limit: Option<u64>, maximum_gas_limit: u64) -> EstimateGasLimits {
        match self {
            // Preserve the legacy behavior exactly: a request gas value above
            // the configured maximum expands the initial search ceiling.
            Self::Default(_) => EstimateGasLimits {
                execution_hard_cap: maximum_gas_limit,
                search_cap: request_gas_limit
                    .map(|request_limit| request_limit.max(maximum_gas_limit))
                    .unwrap_or(maximum_gas_limit),
            },
            Self::Arc(_) => {
                let search_cap = request_gas_limit.unwrap_or(u64::MAX).min(maximum_gas_limit);
                EstimateGasLimits {
                    execution_hard_cap: maximum_gas_limit,
                    search_cap,
                }
            }
        }
    }

    fn cap_buffered_estimate(self, buffered: u64, search_cap: u64) -> u64 {
        if self.is_arc() {
            buffered.min(search_cap)
        } else {
            buffered
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GasLimitErrorClass {
    TooHigh,
    TooLow,
    Other,
}

fn classify_gas_limit_error(error: &InvalidTransaction) -> GasLimitErrorClass {
    match error {
        InvalidTransaction::CallerGasLimitMoreThanBlock
        | InvalidTransaction::TxGasLimitGreaterThanCap { .. } => GasLimitErrorClass::TooHigh,
        InvalidTransaction::CallGasCostMoreThanGasLimit { .. }
        | InvalidTransaction::GasFloorMoreThanGasLimit { .. } => GasLimitErrorClass::TooLow,
        _ => GasLimitErrorClass::Other,
    }
}

fn gas_required_exceeds_allowance_error() -> jsonrpsee::types::ErrorObjectOwned {
    rpc_error_with_code(
        DebankErrorCode::GasExhausted as i32,
        "Invalid gas limit".to_string(),
    )
}

fn arc_gas_allowance<T, StateDB>(tx: &T, db: &StateDB, search_cap: u64) -> RpcResult<u64>
where
    T: TransactionTrait,
    StateDB: DatabaseRef,
{
    let caller = db.basic_ref(tx.caller()).map_err(|error| {
        rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, error.to_string())
    })?;
    let balance = caller.map(|account| account.balance).unwrap_or_default();
    let spendable = balance.checked_sub(tx.value()).ok_or_else(|| {
        rpc_error_with_code(
            DebankErrorCode::BalanceExhausted as i32,
            "Insufficient funds".to_string(),
        )
    })?;

    if tx.gas_price() == 0 {
        return Ok(search_cap);
    }

    let allowance = spendable
        .checked_div(U256::from(tx.gas_price()))
        .unwrap_or_default()
        .min(U256::from(search_cap));
    u64::try_from(allowance).map_err(|_| internal_rpc_err("Arc gas allowance does not fit in u64"))
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
        let tx = self.inner.create_txn_env(
            block,
            block_env,
            request,
            db,
            self.inner.evm_cfg().cfg.chain_id,
        )?;
        let mut res: DebankSingleCallResult = self
            .inner
            .transact(block_env, db, tx)
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
            super::utils::apply_state_overrides(state_override, &mut cache_db)?;
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
                &state,
                &block,
                &block_env,
                &db,
                request,
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
                &block,
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

    fn arc_estimate_retry_at_hard_cap<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: &StateDB,
        mut tx: <C as EvmExecutor>::Tx,
        execution_hard_cap: u64,
    ) -> RpcResult<U256>
    where
        StateDB: DatabaseRef + std::fmt::Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        tx.set_gas_limit(execution_hard_cap);
        let retry = self
            .inner
            .transact_for_estimate(block_env, state, tx, execution_hard_cap)
            .map_err(|error| error.to_rpc_error())?;

        match retry {
            ExecutionResult::Success { .. } => Err(gas_required_exceeds_allowance_error()),
            ExecutionResult::Revert { output, .. } => {
                let reason =
                    decode_revert_reason(&output).unwrap_or("execution revert".to_string());
                Err(rpc_error_with_code(
                    DebankErrorCode::EvmRevert as i32,
                    reason,
                ))
            }
            ExecutionResult::Halt { reason, .. } => {
                let code = DebankErrorCode::from(reason.clone());
                Err(rpc_error_with_code(
                    code as i32,
                    format!("Halted: {:?}", reason),
                ))
            }
        }
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
        let estimate_policy = self.inner.estimate_gas_policy();
        // set nonce to None so that the correct nonce is chosen by the EVM
        request.nonce = None;
        let mut block_env = block_env_from_block(&block);
        let mut cache_db = CacheDB::new(EvmStorageWrapper {
            db: state,
            ovm_address: self.inner.evm_cfg().ovm_address.clone(),
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        });
        if estimate_policy.is_arc() {
            block_env = build_arc_query_environment(
                block.header.clone(),
                block_overrides.clone(),
                ArcQueryKind::CallLike,
                None,
                &mut cache_db,
            )
            .map_err(|error| invalid_params_rpc_err(error.to_string()))?
            .block_env;
        } else if let Some(overrides) = block_overrides.clone() {
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
        let tx_request_gas_price = request.gas_price;
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
        let limits = estimate_policy.limits(tx_request_gas_limit, max_gas_limit);
        let execution_hard_cap = limits.execution_hard_cap;
        let search_cap = limits.search_cap;
        let mut highest_gas_limit = search_cap;
        let mut tx = self.inner.create_txn_env(
            &block,
            &block_env,
            request.clone(),
            &memory_db,
            self.inner.evm_cfg().cfg.chain_id,
        )?;
        tx.set_gas_estimation();
        if estimate_policy.is_arc() {
            highest_gas_limit =
                highest_gas_limit.min(arc_gas_allowance(&tx, &memory_db, search_cap)?);
        }
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
                        // Match Reth's basic-transfer shortcut exactly: the 21,000-gas
                        // probe intentionally ignores a lower gas value in the request.
                        tx.set_gas_limit(MIN_TRANSACTION_GAS);
                        if let Ok(exec_res) = self.inner.transact_for_estimate(
                            &block_env,
                            &memory_db,
                            tx.clone(),
                            execution_hard_cap,
                        ) {
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
        if !estimate_policy.is_arc() && tx.gas_price() > 0 {
            let gas_limit = self
                .inner
                .gas_allowance(&request, &tx, &memory_db, &block_env)?;
            highest_gas_limit = highest_gas_limit.min(gas_limit);
        }
        tx.set_gas_limit(tx.gas_limit().min(highest_gas_limit));

        let res = match self.inner.transact_for_estimate(
            &block_env,
            &memory_db,
            tx.clone(),
            execution_hard_cap,
        ) {
            Ok(result) => result,
            Err(error) if estimate_policy.is_arc() => {
                let error_class = error
                    .get_transaction_error()
                    .as_ref()
                    .map(classify_gas_limit_error)
                    .unwrap_or(GasLimitErrorClass::Other);
                match error_class {
                    GasLimitErrorClass::TooHigh
                        if tx_request_gas_limit.is_some() || tx_request_gas_price.is_some() =>
                    {
                        return self.arc_estimate_retry_at_hard_cap(
                            &block_env,
                            &memory_db,
                            tx,
                            execution_hard_cap,
                        );
                    }
                    GasLimitErrorClass::TooLow => {
                        return Err(gas_required_exceeds_allowance_error());
                    }
                    _ => return Err(error.to_rpc_error()),
                }
            }
            Err(error) => return Err(error.to_rpc_error()),
        };

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
                if estimate_policy.is_arc()
                    && (tx_request_gas_limit.is_some() || tx_request_gas_price.is_some())
                {
                    return self.arc_estimate_retry_at_hard_cap(
                        &block_env,
                        &memory_db,
                        tx,
                        execution_hard_cap,
                    );
                }
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
                .transact_for_estimate(&block_env, &memory_db, tx.clone(), execution_hard_cap)
                .map_err(|e| e.to_rpc_error())?;
            gas_used = res.gas_used();
            update_estimated_gas_range_for_policy(
                estimate_policy,
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

            let res = self.inner.transact_for_estimate(
                &block_env,
                &memory_db,
                tx.clone(),
                execution_hard_cap,
            );

            match res {
                Err(e) => {
                    if let Some(invalid_tx_err) = e.get_transaction_error() {
                        match classify_gas_limit_error(&invalid_tx_err) {
                            GasLimitErrorClass::TooHigh => {
                                highest_gas_limit = mid_gas_limit;
                            }
                            GasLimitErrorClass::TooLow => {
                                lowest_gas_limit = mid_gas_limit;
                            }
                            GasLimitErrorClass::Other => {
                                if estimate_policy.is_arc() {
                                    return Err(e.to_rpc_error());
                                }
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
                    update_estimated_gas_range_for_policy(
                        estimate_policy,
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
            estimate_policy.cap_buffered_estimate(buffered.min(u64::MAX as u128) as u64, search_cap)
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
    use crate::api_impl::ApiImpl;
    use alloy::primitives::keccak256;
    use alloy::rpc::types::{TransactionInput, TransactionRequest};
    use leafage_evm_chains::arc::{ArcChainConfig, ARC_MAINNET_CHAIN_ID};
    use leafage_evm_storage::{
        EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig,
        StorageKind,
    };
    use leafage_evm_types::{
        AccountStorageDiff, Block, BlockStorageDiff, CfgEnv, IndexValuePair, MainnetSpecId,
        NewAccount, NewCode,
    };
    use revm::primitives::eip7825::TX_GAS_LIMIT_CAP;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    type ArcTestApi = Api<ApiImpl<Arc<StateTree<MultiStorage>>, MainnetSpecId, ArcChainConfig>>;

    const ARC_RPC_GAS_CAP: u64 = 25_000_000;
    const BLOCK_GAS_LIMIT: u64 = 30_000_000;
    const ANCHOR_NUMBER: u64 = 1;
    const ANCHOR_BASE_FEE: u64 = 3;
    const NEXT_BASE_FEE: u64 = 7;
    const EIP7825_SUCCESS_CALLDATA_BYTES: usize = 418_905;
    const EIP7825_FAILURE_CALLDATA_BYTES: usize = 418_906;

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Copy)]
    struct TestAddresses {
        funded: Address,
        limited: Address,
        empty: Address,
        blocked: Address,
        gas_guard: Address,
        revert: Address,
        environment: Address,
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
            0x62,
            threshold[1],
            threshold[2],
            threshold[3],
            0x90,
            0x10,
            0x60,
            0x0b,
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

    fn blocklist_storage_index(address: Address) -> H256 {
        let mut mapping_input = [0u8; 64];
        mapping_input[12..32].copy_from_slice(address.as_slice());
        mapping_input[63] = 2;
        keccak256(keccak256(mapping_input))
    }

    fn build_arc_fixture(estimate_gas_buffer: u64) -> ArcFixture {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("leafage-arc-estimate-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();

        let addresses = TestAddresses {
            funded: Address::repeat_byte(0x11),
            limited: Address::repeat_byte(0x12),
            empty: Address::repeat_byte(0x22),
            blocked: Address::repeat_byte(0x33),
            gas_guard: Address::repeat_byte(0x44),
            revert: Address::repeat_byte(0x55),
            environment: Address::repeat_byte(0x66),
        };
        let native_coin_control: Address = "0x1800000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let gas_guard = gas_guard_code(300_000);
        let revert = Bytes::from_static(&[0x5f, 0x5f, 0xfd]);
        let environment = environment_code();

        let mut diff = BlockStorageDiff::default();
        for (address, balance, nonce, code_hash) in [
            (addresses.funded, U256::ONE << 128, 0, H256::ZERO),
            (addresses.limited, U256::from(250_000u64), 0, H256::ZERO),
            (
                addresses.blocked,
                U256::from(1_000_000_000u64),
                0,
                H256::ZERO,
            ),
            (native_coin_control, U256::ZERO, 1, H256::ZERO),
            (addresses.gas_guard, U256::ZERO, 1, keccak256(&gas_guard)),
            (addresses.revert, U256::ZERO, 1, keccak256(&revert)),
            (
                addresses.environment,
                U256::ZERO,
                1,
                keccak256(&environment),
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
                code_hash: keccak256(&gas_guard),
                code: gas_guard,
            },
            NewCode {
                code_hash: keccak256(&revert),
                code: revert,
            },
            NewCode {
                code_hash: keccak256(&environment),
                code: environment,
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
        StateDBWrapper(
            db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                .unwrap()
                .unwrap(),
        )
        .update_block(
            test_block(0, H256::repeat_byte(0xaa), H256::ZERO, ANCHOR_BASE_FEE),
            diff,
        )
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
        cfg.tx_gas_limit_cap = Some(ARC_RPC_GAS_CAP);
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
        let block_env = build_arc_query_environment(
            block.header.clone(),
            overrides,
            ArcQueryKind::CallLike,
            None,
            &mut cache_db,
        )
        .unwrap()
        .block_env;
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
        api.inner
            .transact_for_estimate(&block_env, &memory_db, tx, gas_limit)
            .unwrap()
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
    fn policies_keep_default_behavior_and_arc_hard_cap() {
        let default = EstimateGasPolicy::default();
        let default_limits = default.limits(Some(40_000_000), 30_000_000);
        assert_eq!(default_limits.execution_hard_cap, 30_000_000);
        assert_eq!(default_limits.search_cap, 40_000_000);
        assert_eq!(
            default.cap_buffered_estimate(60_000_000, 30_000_000),
            60_000_000
        );

        let arc = EstimateGasPolicy::Arc(crate::api_impl::core::ArcEstimateGasPolicy);
        let arc_limits = arc.limits(Some(100_000), TX_GAS_LIMIT_CAP);
        assert_eq!(arc_limits.execution_hard_cap, TX_GAS_LIMIT_CAP);
        assert_eq!(arc_limits.search_cap, 100_000);
        assert_eq!(
            arc.cap_buffered_estimate(200_000, arc_limits.search_cap),
            arc_limits.search_cap
        );
    }

    #[test]
    fn arc_estimate_handles_transfer_value_fee_and_large_balance() {
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
                .to(addresses.empty)
                .gas_price(1),
            tempo: None,
        };
        assert_eq!(
            estimate(&fixture.api, large_balance_allowance, None).unwrap(),
            U256::from(MIN_TRANSACTION_GAS)
        );

        fixture.close();
    }

    #[test]
    fn arc_estimate_enforces_eip7825_boundaries_and_recaps_buffer() {
        let fixture = build_arc_fixture(200);
        let addresses = fixture.addresses;
        let request = |calldata_bytes| CallRequest {
            inner: TransactionRequest::default()
                .from(addresses.funded)
                .to(addresses.empty)
                .gas_limit(ARC_RPC_GAS_CAP)
                .input(TransactionInput::new(Bytes::from(vec![1; calldata_bytes]))),
            tempo: None,
        };

        let estimate_at_cap =
            estimate(&fixture.api, request(EIP7825_SUCCESS_CALLDATA_BYTES), None).unwrap();
        assert_eq!(estimate_at_cap, U256::from(TX_GAS_LIMIT_CAP));
        assert!(matches!(
            execute_arc_estimate_probe(
                &fixture.api,
                request(EIP7825_SUCCESS_CALLDATA_BYTES),
                None,
                TX_GAS_LIMIT_CAP,
            ),
            ExecutionResult::Success { .. }
        ));

        let error =
            estimate(&fixture.api, request(EIP7825_FAILURE_CALLDATA_BYTES), None).unwrap_err();
        assert_eq!(error.code(), DebankErrorCode::GasExhausted as i32);
        assert_eq!(error.message(), "Invalid gas limit");

        fixture.close();
    }

    #[test]
    fn arc_estimate_handles_gas_dependent_revert_and_returns_executable_gas() {
        let fixture = build_arc_fixture(100);
        let request = call_request(fixture.addresses.funded, fixture.addresses.gas_guard);

        let explicitly_too_low = CallRequest {
            inner: TransactionRequest::default()
                .from(fixture.addresses.funded)
                .to(fixture.addresses.gas_guard)
                .gas_limit(250_000),
            tempo: None,
        };
        let error = estimate(&fixture.api, explicitly_too_low, None).unwrap_err();
        assert_eq!(error.code(), DebankErrorCode::GasExhausted as i32);
        assert_eq!(error.message(), "Invalid gas limit");

        let fee_limited = CallRequest {
            inner: TransactionRequest::default()
                .from(fixture.addresses.limited)
                .to(fixture.addresses.gas_guard)
                .gas_price(1),
            tempo: None,
        };
        let error = estimate(&fixture.api, fee_limited, None).unwrap_err();
        assert_eq!(error.code(), DebankErrorCode::BalanceExhausted as i32);
        assert_eq!(error.message(), "Insufficient funds");

        let estimated: u64 = estimate(&fixture.api, request.clone(), None)
            .unwrap()
            .try_into()
            .unwrap();
        assert!(estimated < TX_GAS_LIMIT_CAP);
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
    fn arc_estimate_uses_h_and_h_plus_one_query_environment_and_overrides() {
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

        let next = BlockOverrides::default().with_number(U256::from(ANCHOR_NUMBER + 1));
        let next_gas: u64 = estimate(&fixture.api, request.clone(), Some(next.clone()))
            .unwrap()
            .try_into()
            .unwrap();
        let next_output = success_output(execute_arc_estimate_probe(
            &fixture.api,
            request.clone(),
            Some(next),
            next_gas,
        ));
        let next_words = output_words(&next_output);
        assert_eq!(next_words[0], U256::from(ANCHOR_NUMBER + 1));
        assert_eq!(next_words[1], U256::from(NEXT_BASE_FEE));
        assert_eq!(
            next_words[3],
            U256::from_be_slice(H256::repeat_byte(0xbb).as_slice())
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
fn update_estimated_gas_range_for_policy<R: GetHaltReason + Clone>(
    policy: EstimateGasPolicy,
    result: &ExecutionResult<R>,
    tx_gas_limit: u64,
    highest_gas_limit: &mut u64,
    lowest_gas_limit: &mut u64,
) -> RpcResult<()> {
    if policy.is_arc() {
        match result {
            ExecutionResult::Success { .. } => *highest_gas_limit = tx_gas_limit,
            // Reth treats every failed lower-gas probe as evidence that the
            // lower bound must increase. Some contracts use REVERT or INVALID
            // for gas-dependent failure instead of an OOG halt.
            ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. } => {
                *lowest_gas_limit = tx_gas_limit
            }
        }
        Ok(())
    } else {
        update_estimated_gas_range(result, tx_gas_limit, highest_gas_limit, lowest_gas_limit)
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
