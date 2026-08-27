//! Handler for the BlockX-internal `blockx_stateReadBatch` method.
//!
//! One batch resolves N `getAddressCode` / `getStorageAt` /
//! `getAddressBalance` / `getAddressNonce` reads against a single state
//! view of a fixed block: one `state_at`, deduplicated keys, and
//! batched storage reads (RocksDB MultiGet on the non-archive backend,
//! scalar fallback elsewhere). Code, balance and nonce reads share one
//! account pool — the same address requested by several kinds costs one
//! account read. The wire payload is BSRB/1 binary in a JSON-RPC hex
//! shell; item order is the correlation. Per-item results keep the
//! exact value shapes and error code/message text of the single methods
//! — BlockX's provider forwards item errors verbatim and leafage-py
//! parses -39006/-39007 message text.

use super::debank::combine_error_message;
use super::utils;
use crate::api::{BlockxApiServer, DebankApiClient};
use crate::api_impl::core::{
    Api, ApiCore, EvmExecutor, GetHaltReason, GetTransactionError, ToJsonRpcError,
};
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use jsonrpsee::core::RpcResult;
use jsonrpsee::http_client::HttpClient;
use leafage_evm_storage::{BlockIndex, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{
    Address, BlockId, BlockNumberOrTag, BlockType, BsrbContext, BsrbOutcome, BsrbRead, BsrbRequest,
    BsrbResponse, Bytes, DebankBlockContext, DebankErrorCode, JsonStorageKey, H256,
    KECCAK256_EMPTY, U256,
};
use metrics::{counter, histogram};
use revm::database::DatabaseRef;
use std::collections::HashMap;
use std::time::Instant;

/// Item error mirroring the single-method error object; encoded into
/// the BSRB error tag verbatim.
#[derive(Clone, Debug)]
struct ItemError {
    code: i32,
    message: String,
}

fn item_error(err: ItemError) -> BsrbOutcome {
    BsrbOutcome::Error {
        code: err.code,
        message: err.message,
    }
}

/// `getStorageAt` value shape: the full 32-byte word.
fn storage_value(word: H256) -> Bytes {
    Bytes::copy_from_slice(word.as_slice())
}

/// `getAddressBalance` / `getAddressNonce` value shape: the quantity as
/// minimal big-endian bytes (empty for zero). The Go facade renders the
/// single-method `"0x…"` quantity text from these.
fn quantity_value(value: U256) -> Bytes {
    Bytes::from(value.to_be_bytes_trimmed_vec())
}

/// The `DebankBlockContext` equivalent of a BSRB context: always
/// `Equals` on a fixed hash or height. Dynamic contexts are
/// unrepresentable on the wire by construction.
fn block_context(context: &BsrbContext) -> DebankBlockContext {
    DebankBlockContext {
        block_id: match context {
            BsrbContext::Hash(hash) => BlockId::Hash((*hash).into()),
            BsrbContext::Number(number) => BlockId::Number(BlockNumberOrTag::Number(*number)),
        },
        block_type: BlockType::Equals,
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
    /// Blocking part: one `state_at`, then batched, deduplicated reads.
    /// A failed batched read degrades to the scalar path so errors stay
    /// attributable per item with the single-method code/message text.
    fn blockx_state_read_batch_inner(
        &self,
        batch: &BsrbRequest,
        block_ctx: &DebankBlockContext,
    ) -> RpcResult<Vec<BsrbOutcome>> {
        let total_start = Instant::now();
        let stage_start = Instant::now();
        let state = self.debank_get_state_by_ctx_impl(Some(block_ctx.clone()))?;
        histogram!("leafage_state_batch_latency_seconds", "stage" => "state_at")
            .record(stage_start.elapsed().as_secs_f64());
        let state = EvmStorageWrapper {
            db: state,
            ovm_address: self.inner.evm_cfg().ovm_address.clone(),
            normalize_state_key: self.inner.evm_cfg().normalize_state_key,
        };

        // Per-kind logical item counts, before deduplication.
        let mut kind_counts = [0usize; 4];
        for read in &batch.reads {
            kind_counts[match read {
                BsrbRead::AddressCode { .. } => 0,
                BsrbRead::StorageAt { .. } => 1,
                BsrbRead::Balance { .. } => 2,
                BsrbRead::Nonce { .. } => 3,
            }] += 1;
        }
        for (kind, count) in ["addressCode", "storageAt", "balance", "nonce"]
            .into_iter()
            .zip(kind_counts)
        {
            histogram!("leafage_state_batch_size", "kind" => kind).record(count as f64);
        }

        // Deduplicate keys while remembering, for every read, which
        // unique slot it resolves from. addressCode, balance and nonce
        // share one account pool — the same address asked for by
        // several kinds costs one account read — and only addresses
        // actually asked for code join the code-hash fetch below.
        let mut account_addresses: Vec<Address> = Vec::new();
        let mut account_slots: HashMap<Address, usize> = HashMap::new();
        let mut account_needs_code: Vec<bool> = Vec::new();
        let mut storage_keys: Vec<(Address, H256)> = Vec::new();
        let mut storage_slots: HashMap<(Address, H256), usize> = HashMap::new();
        for read in &batch.reads {
            match read {
                BsrbRead::AddressCode { address } => {
                    let slot = *account_slots.entry(*address).or_insert_with(|| {
                        account_addresses.push(*address);
                        account_needs_code.push(false);
                        account_addresses.len() - 1
                    });
                    account_needs_code[slot] = true;
                }
                BsrbRead::StorageAt { address, slot } => {
                    let key = (*address, *slot);
                    storage_slots.entry(key).or_insert_with(|| {
                        storage_keys.push(key);
                        storage_keys.len() - 1
                    });
                }
                BsrbRead::Balance { address } | BsrbRead::Nonce { address } => {
                    account_slots.entry(*address).or_insert_with(|| {
                        account_addresses.push(*address);
                        account_needs_code.push(false);
                        account_addresses.len() - 1
                    });
                }
            }
        }

        // Storage values, per unique (address, slot).
        let stage_start = Instant::now();
        let storage_index_keys: Vec<(Address, U256)> = storage_keys
            .iter()
            .map(|(address, slot)| (*address, U256::from_be_bytes((*slot).into())))
            .collect();
        let storage_results: Vec<Result<U256, ItemError>> =
            match state.storage_many_ref(&storage_index_keys) {
                Ok(values) => values.into_iter().map(Ok).collect(),
                // The batched read failed as a whole; re-read each key on
                // the scalar path so failures are attributed per item with
                // the exact single-method error text.
                Err(_) => storage_keys
                    .iter()
                    .map(|(address, slot)| {
                        state
                            .storage_ref(address.0.into(), U256::from_be_bytes((*slot).into()))
                            .map_err(|e| ItemError {
                                code: jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                                message: format!(
                                    "Failed to get storage at {:?} {:?}: {:?}",
                                    address, slot, e
                                ),
                            })
                    })
                    .collect(),
            };
        histogram!("leafage_state_batch_latency_seconds", "stage" => "multiget_storage")
            .record(stage_start.elapsed().as_secs_f64());

        // Account infos, then deduplicated code fetches by code hash.
        let stage_start = Instant::now();
        let account_results = match state.basic_many_ref(&account_addresses) {
            Ok(values) => values.into_iter().map(Ok).collect::<Vec<_>>(),
            Err(_) => account_addresses
                .iter()
                .map(|address| {
                    state.basic_ref(address.0.into()).map_err(|e| ItemError {
                        code: DebankErrorCode::DataBaseFailed as i32,
                        message: e.to_string(),
                    })
                })
                .collect(),
        };
        histogram!("leafage_state_batch_latency_seconds", "stage" => "multiget_account")
            .record(stage_start.elapsed().as_secs_f64());

        let stage_start = Instant::now();
        let mut code_hashes: Vec<H256> = Vec::new();
        let mut code_hash_slots: HashMap<H256, usize> = HashMap::new();
        for (account, needs_code) in account_results
            .iter()
            .zip(account_needs_code.iter().copied())
        {
            if !needs_code {
                continue;
            }
            if let Ok(Some(account)) = account {
                if !account.code_hash.is_zero() && account.code_hash != KECCAK256_EMPTY {
                    code_hash_slots.entry(account.code_hash).or_insert_with(|| {
                        code_hashes.push(account.code_hash);
                        code_hashes.len() - 1
                    });
                }
            }
        }
        let code_hash_results = match state.code_by_hash_many_ref(&code_hashes) {
            Ok(values) => values.into_iter().map(Ok).collect::<Vec<_>>(),
            Err(_) => code_hashes
                .iter()
                .map(|hash| {
                    state.code_by_hash_ref(*hash).map_err(|e| ItemError {
                        code: DebankErrorCode::DataBaseFailed as i32,
                        message: e.to_string(),
                    })
                })
                .collect(),
        };
        histogram!("leafage_state_batch_latency_seconds", "stage" => "multiget_code")
            .record(stage_start.elapsed().as_secs_f64());

        // Per unique address that was asked for code: the same
        // empty-code rules as getAddressCode. Entries for balance/nonce
        // -only addresses are placeholders and never read.
        let code_results: Vec<Result<Bytes, ItemError>> = account_results
            .iter()
            .zip(account_needs_code.iter().copied())
            .map(|(account, needs_code)| {
                if !needs_code {
                    return Ok(Bytes::new());
                }
                let account = match account {
                    Ok(account) => account,
                    Err(err) => return Err(err.clone()),
                };
                let Some(account) = account else {
                    return Ok(Bytes::new());
                };
                if account.code_hash.is_zero() || account.code_hash == KECCAK256_EMPTY {
                    return Ok(Bytes::new());
                }
                match &code_hash_results[code_hash_slots[&account.code_hash]] {
                    Ok(code) => Ok(code.original_bytes().0.clone().into()),
                    Err(err) => Err(err.clone()),
                }
            })
            .collect();

        let outcomes = batch
            .reads
            .iter()
            .map(|read| match read {
                BsrbRead::AddressCode { address } => match &code_results[account_slots[address]] {
                    Ok(code) => BsrbOutcome::Value(code.clone()),
                    Err(err) => item_error(err.clone()),
                },
                BsrbRead::StorageAt { address, slot } => {
                    match &storage_results[storage_slots[&(*address, *slot)]] {
                        Ok(value) => {
                            let raw: [u8; 32] = value.to_be_bytes();
                            BsrbOutcome::Value(storage_value(raw.into()))
                        }
                        Err(err) => item_error(err.clone()),
                    }
                }
                BsrbRead::Balance { address } => match &account_results[account_slots[address]] {
                    Ok(account) => BsrbOutcome::Value(quantity_value(
                        account.as_ref().map(|a| a.balance).unwrap_or_default(),
                    )),
                    Err(err) => item_error(err.clone()),
                },
                BsrbRead::Nonce { address } => match &account_results[account_slots[address]] {
                    Ok(account) => BsrbOutcome::Value(quantity_value(U256::from(
                        account.as_ref().map(|a| a.nonce).unwrap_or_default(),
                    ))),
                    Err(err) => item_error(err.clone()),
                },
            })
            .collect();
        histogram!("leafage_state_batch_latency_seconds", "stage" => "total")
            .record(total_start.elapsed().as_secs_f64());
        Ok(outcomes)
    }

    /// Per-item historical retry for items that failed locally, keeping
    /// the exact single-method fallback semantics (`should_try_historical`
    /// gate, combined error message). Successful local items are never
    /// re-read.
    async fn blockx_retry_items_via_historical(
        &self,
        batch: &BsrbRequest,
        mut outcomes: Vec<BsrbOutcome>,
        block_ctx: &Option<DebankBlockContext>,
    ) -> Vec<BsrbOutcome> {
        let Some(client) = self.should_try_historical(block_ctx) else {
            return outcomes;
        };
        let failed: Vec<usize> = outcomes
            .iter()
            .enumerate()
            .filter_map(|(pos, outcome)| {
                matches!(outcome, BsrbOutcome::Error { .. }).then_some(pos)
            })
            .collect();
        if failed.is_empty() {
            return outcomes;
        }
        let retries = failed
            .iter()
            .map(|&pos| historical_item(client, &batch.reads[pos], block_ctx));
        for (&pos, retried) in failed.iter().zip(futures::future::join_all(retries).await) {
            match retried {
                Ok(value) => outcomes[pos] = BsrbOutcome::Value(value),
                Err(historical_err) => {
                    let BsrbOutcome::Error { code, message } = &outcomes[pos] else {
                        unreachable!("failed item carries an error");
                    };
                    outcomes[pos] = BsrbOutcome::Error {
                        code: *code,
                        message: combine_error_message(message, &historical_err),
                    };
                }
            }
        }
        outcomes
    }

    /// Whole-batch local failure (no state view): resolve every item
    /// through the historical client; items it cannot serve carry the
    /// combined local+historical error under the local error code.
    async fn blockx_all_items_via_historical(
        &self,
        batch: &BsrbRequest,
        local_err: &jsonrpsee::types::ErrorObjectOwned,
        block_ctx: &Option<DebankBlockContext>,
    ) -> Vec<BsrbOutcome> {
        let client = self
            .should_try_historical(block_ctx)
            .expect("caller checked historical availability");
        let retries = batch
            .reads
            .iter()
            .map(|read| historical_item(client, read, block_ctx));
        futures::future::join_all(retries)
            .await
            .into_iter()
            .map(|retried| match retried {
                Ok(value) => BsrbOutcome::Value(value),
                Err(historical_err) => BsrbOutcome::Error {
                    code: local_err.code(),
                    message: combine_error_message(local_err.message(), &historical_err),
                },
            })
            .collect()
    }

    /// The full resolution pipeline for one batch: state-read
    /// admission, one blocking local pass, then per-item or whole-batch
    /// historical fallback. `Err` is a batch-level failure with no
    /// eligible historical client (same gate as the single methods).
    async fn blockx_resolve_batch(&self, batch: &BsrbRequest) -> RpcResult<Vec<BsrbOutcome>> {
        // State-read admission: wait on the async side (cancellable),
        // move the permit into the blocking task so it is held until the
        // reads finish. One batch takes one permit; the per-batch item
        // cap bounds the work behind it.
        let permit = match self.inner.evm_cfg().state_read_limiter.clone() {
            Some(limiter) => {
                let wait_start = Instant::now();
                // acquire_owned only errors when the semaphore is closed,
                // which never happens here.
                let permit = limiter.acquire_owned().await.ok();
                histogram!("leafage_state_read_queue_wait_seconds")
                    .record(wait_start.elapsed().as_secs_f64());
                permit
            }
            None => None,
        };
        let context = block_context(&batch.context);
        let this = self.clone();
        let local_batch = batch.clone();
        let local_ctx = context.clone();
        let local = utils::spawn_blocking_with_cancel(move |_token| {
            let _permit = permit;
            this.blockx_state_read_batch_inner(&local_batch, &local_ctx)
        })
        .await
        .map_err(|_| internal_rpc_err("state read batch failed"))?;

        let block_ctx = Some(context);
        match local {
            Ok(outcomes) => Ok(self
                .blockx_retry_items_via_historical(batch, outcomes, &block_ctx)
                .await),
            Err(local_err) => {
                // Same fallback gate as the single methods: without an
                // eligible historical client the batch-level error (e.g.
                // -39006/-39007 from state resolution) is returned as-is.
                if self.should_try_historical(&block_ctx).is_none() {
                    return Err(local_err);
                }
                Ok(self
                    .blockx_all_items_via_historical(batch, &local_err, &block_ctx)
                    .await)
            }
        }
    }

    /// Virtual-balance chains: `getAddressBalance` answers before any
    /// state resolution, so balance items are fixed up front and only
    /// the remaining reads resolve against state. This holds on every
    /// path — balance items still answer when the batch's block cannot
    /// be resolved locally (state-dependent items then fail per item
    /// with the batch-level code/message each single method would
    /// return), and they are never re-read through the historical
    /// client.
    async fn blockx_resolve_with_virtual_balance(
        &self,
        batch: &BsrbRequest,
        vb: U256,
    ) -> Vec<BsrbOutcome> {
        let balance = BsrbOutcome::Value(quantity_value(vb));
        let rest: Vec<BsrbRead> = batch
            .reads
            .iter()
            .copied()
            .filter(|read| !matches!(read, BsrbRead::Balance { .. }))
            .collect();
        let rest_outcomes = if rest.is_empty() {
            Vec::new()
        } else {
            let count = rest.len();
            let sub = BsrbRequest {
                context: batch.context,
                reads: rest,
            };
            match self.blockx_resolve_batch(&sub).await {
                Ok(outcomes) => outcomes,
                Err(err) => vec![
                    BsrbOutcome::Error {
                        code: err.code(),
                        message: err.message().to_string(),
                    };
                    count
                ],
            }
        };
        let mut rest_outcomes = rest_outcomes.into_iter();
        batch
            .reads
            .iter()
            .map(|read| match read {
                BsrbRead::Balance { .. } => balance.clone(),
                _ => rest_outcomes
                    .next()
                    .expect("one outcome per non-balance read"),
            })
            .collect()
    }
}

async fn historical_item(
    client: &HttpClient,
    read: &BsrbRead,
    block_ctx: &Option<DebankBlockContext>,
) -> Result<Bytes, jsonrpsee::core::ClientError> {
    match read {
        BsrbRead::AddressCode { address } => {
            client.get_address_code(*address, block_ctx.clone()).await
        }
        BsrbRead::StorageAt { address, slot } => client
            .get_storage_at(*address, JsonStorageKey::from(*slot), block_ctx.clone())
            .await
            .map(storage_value),
        BsrbRead::Balance { address } => client
            .get_address_balance(*address, block_ctx.clone())
            .await
            .map(quantity_value),
        BsrbRead::Nonce { address } => client
            .get_address_nonce(*address, block_ctx.clone())
            .await
            .map(quantity_value),
    }
}

#[async_trait::async_trait]
impl<C> BlockxApiServer for Api<C>
where
    C: ApiCore,
    C::DB: EvmStorageRead + BlockIndex,
    C::TransactionError: ToJsonRpcError + GetTransactionError,
    C::EvmHaltReason: std::fmt::Debug + Clone + GetHaltReason,
    DebankErrorCode: From<<C as EvmExecutor>::EvmHaltReason>,
{
    async fn state_read_batch(&self, payload: Bytes) -> RpcResult<Bytes> {
        // Strict binary decoding doubles as request validation: version,
        // context kind, read kinds, item count and exact length are all
        // enforced before any state work.
        let batch = match BsrbRequest::decode(&payload) {
            Ok(batch) => batch,
            Err(err) => {
                counter!("leafage_state_batch_requests_total", "outcome" => "invalid").increment(1);
                return Err(invalid_params_rpc_err(err.to_string()));
            }
        };

        let results = match self.inner.virtual_balance() {
            Some(vb) => self.blockx_resolve_with_virtual_balance(&batch, vb).await,
            None => match self.blockx_resolve_batch(&batch).await {
                Ok(results) => results,
                Err(err) => {
                    counter!("leafage_state_batch_requests_total", "outcome" => "error")
                        .increment(1);
                    return Err(err);
                }
            },
        };
        counter!("leafage_state_batch_requests_total", "outcome" => "ok").increment(1);
        Ok(Bytes::from(BsrbResponse { results }.encode()))
    }
}
