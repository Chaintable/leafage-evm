//! Handler for the BlockX-internal `blockx_stateReadBatch` method.
//!
//! One batch resolves N `getAddressCode` / `getStorageAt` reads against
//! a single state view of a fixed block: one `state_at`, deduplicated
//! keys, and batched storage reads (RocksDB MultiGet on the non-archive
//! backend, scalar fallback elsewhere). The wire payload is BSRB/1
//! binary in a JSON-RPC hex shell; item order is the correlation.
//! Per-item results keep the exact value shapes and error code/message
//! text of the single methods — BlockX's provider forwards item errors
//! verbatim and leafage-py parses -39006/-39007 message text.

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

        // Deduplicate keys per kind while remembering, for every read,
        // which unique slot it resolves from.
        let mut code_addresses: Vec<Address> = Vec::new();
        let mut code_slots: HashMap<Address, usize> = HashMap::new();
        let mut storage_keys: Vec<(Address, H256)> = Vec::new();
        let mut storage_slots: HashMap<(Address, H256), usize> = HashMap::new();
        for read in &batch.reads {
            match read {
                BsrbRead::AddressCode { address } => {
                    code_slots.entry(*address).or_insert_with(|| {
                        code_addresses.push(*address);
                        code_addresses.len() - 1
                    });
                }
                BsrbRead::StorageAt { address, slot } => {
                    let key = (*address, *slot);
                    storage_slots.entry(key).or_insert_with(|| {
                        storage_keys.push(key);
                        storage_keys.len() - 1
                    });
                }
            }
        }
        histogram!("leafage_state_batch_size", "kind" => "addressCode")
            .record(code_slots.len() as f64);
        histogram!("leafage_state_batch_size", "kind" => "storageAt")
            .record(storage_slots.len() as f64);

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
        let account_results = match state.basic_many_ref(&code_addresses) {
            Ok(values) => values.into_iter().map(Ok).collect::<Vec<_>>(),
            Err(_) => code_addresses
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
        for account in account_results.iter().flatten() {
            if let Some(account) = account {
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

        // Per unique address: the same empty-code rules as getAddressCode.
        let code_results: Vec<Result<Bytes, ItemError>> = account_results
            .into_iter()
            .map(|account| {
                let account = match account {
                    Ok(account) => account,
                    Err(err) => return Err(err),
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
                BsrbRead::AddressCode { address } => match &code_results[code_slots[address]] {
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
        let results = match local {
            Ok(outcomes) => {
                self.blockx_retry_items_via_historical(&batch, outcomes, &block_ctx)
                    .await
            }
            Err(local_err) => {
                // Same fallback gate as the single methods: without an
                // eligible historical client the batch-level error (e.g.
                // -39006/-39007 from state resolution) is returned as-is.
                if self.should_try_historical(&block_ctx).is_none() {
                    counter!("leafage_state_batch_requests_total", "outcome" => "error")
                        .increment(1);
                    return Err(local_err);
                }
                self.blockx_all_items_via_historical(&batch, &local_err, &block_ctx)
                    .await
            }
        };
        counter!("leafage_state_batch_requests_total", "outcome" => "ok").increment(1);
        Ok(Bytes::from(BsrbResponse { results }.encode()))
    }
}
