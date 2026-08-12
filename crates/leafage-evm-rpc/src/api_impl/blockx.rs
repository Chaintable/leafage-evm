//! Handler for the BlockX-internal `blockx_stateReadBatch` method.
//!
//! One batch resolves N `getAddressCode` / `getStorageAt` reads against
//! a single state view of a fixed block: one `state_at`, deduplicated
//! keys, and batched storage reads (RocksDB MultiGet on the non-archive
//! backend, scalar fallback elsewhere). Per-item results keep the exact
//! value shapes and error code/message text of the single methods —
//! leafage-py parses -39006/-39007 messages, so BlockX forwards item
//! errors byte-for-byte.

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
    Address, BlockId, BlockNumberOrTag, BlockType, BlockxStateRead, BlockxStateReadBatch,
    BlockxStateReadBatchResp, BlockxStateReadError, BlockxStateReadOutcome, BlockxStateReadValue,
    Bytes, DebankBlockContext, DebankErrorCode, BLOCKX_STATE_READ_BATCH_MAX_ITEMS, H256,
    KECCAK256_EMPTY, U256,
};
use metrics::{counter, histogram};
use revm::database::DatabaseRef;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

fn ok_outcome(index: u32, value: BlockxStateReadValue) -> BlockxStateReadOutcome {
    BlockxStateReadOutcome {
        index,
        value: Some(value),
        error: None,
    }
}

fn err_outcome(index: u32, error: BlockxStateReadError) -> BlockxStateReadOutcome {
    BlockxStateReadOutcome {
        index,
        value: None,
        error: Some(error),
    }
}

/// Deterministic request validation. Only fixed `Equals` block contexts
/// are accepted: `latest`/`pending`/`Contains` stay on the single-method
/// path where their dynamic-head semantics are well defined.
fn validate_batch(batch: &BlockxStateReadBatch) -> RpcResult<()> {
    if batch.reads.is_empty() {
        return Err(invalid_params_rpc_err("reads must not be empty"));
    }
    if batch.reads.len() > BLOCKX_STATE_READ_BATCH_MAX_ITEMS {
        return Err(invalid_params_rpc_err(format!(
            "reads exceeds the hard cap of {} items",
            BLOCKX_STATE_READ_BATCH_MAX_ITEMS
        )));
    }
    let mut seen = HashSet::with_capacity(batch.reads.len());
    for read in &batch.reads {
        if !seen.insert(read.index()) {
            return Err(invalid_params_rpc_err(format!(
                "duplicate read index {}",
                read.index()
            )));
        }
    }
    if batch.block_context.block_type != BlockType::Equals {
        return Err(invalid_params_rpc_err(
            "blockContext.type must be \"Equals\"",
        ));
    }
    match batch.block_context.block_id {
        BlockId::Hash(_) | BlockId::Number(BlockNumberOrTag::Number(_)) => Ok(()),
        _ => Err(invalid_params_rpc_err(
            "blockContext.block_id must be a fixed block hash or height",
        )),
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
        batch: &BlockxStateReadBatch,
    ) -> RpcResult<Vec<BlockxStateReadOutcome>> {
        let total_start = Instant::now();
        let stage_start = Instant::now();
        let state = self.debank_get_state_by_ctx_impl(Some(batch.block_context.clone()))?;
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
                BlockxStateRead::AddressCode { address, .. } => {
                    code_slots.entry(*address).or_insert_with(|| {
                        code_addresses.push(*address);
                        code_addresses.len() - 1
                    });
                }
                BlockxStateRead::StorageAt {
                    address, position, ..
                } => {
                    let key = (*address, position.as_b256());
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

        // Storage values, per unique (address, position).
        let stage_start = Instant::now();
        let storage_index_keys: Vec<(Address, U256)> = storage_keys
            .iter()
            .map(|(address, position)| (*address, U256::from_be_bytes((*position).into())))
            .collect();
        let storage_results: Vec<Result<U256, BlockxStateReadError>> =
            match state.storage_many_ref(&storage_index_keys) {
                Ok(values) => values.into_iter().map(Ok).collect(),
                // The batched read failed as a whole; re-read each key on
                // the scalar path so failures are attributed per item with
                // the exact single-method error text.
                Err(_) => storage_keys
                    .iter()
                    .map(|(address, position)| {
                        state
                            .storage_ref(address.0.into(), U256::from_be_bytes((*position).into()))
                            .map_err(|e| BlockxStateReadError {
                                code: jsonrpsee::types::error::INTERNAL_ERROR_CODE,
                                message: format!(
                                    "Failed to get storage at {:?} {:?}: {:?}",
                                    address, position, e
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
                    state
                        .basic_ref(address.0.into())
                        .map_err(|e| BlockxStateReadError {
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
                    state
                        .code_by_hash_ref(*hash)
                        .map_err(|e| BlockxStateReadError {
                            code: DebankErrorCode::DataBaseFailed as i32,
                            message: e.to_string(),
                        })
                })
                .collect(),
        };
        histogram!("leafage_state_batch_latency_seconds", "stage" => "multiget_code")
            .record(stage_start.elapsed().as_secs_f64());

        // Per unique address: the same empty-code rules as getAddressCode.
        let code_results: Vec<Result<Bytes, BlockxStateReadError>> = account_results
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
                BlockxStateRead::AddressCode { index, address } => {
                    match &code_results[code_slots[address]] {
                        Ok(code) => ok_outcome(*index, BlockxStateReadValue::code(code.clone())),
                        Err(err) => err_outcome(*index, err.clone()),
                    }
                }
                BlockxStateRead::StorageAt {
                    index,
                    address,
                    position,
                } => match &storage_results[storage_slots[&(*address, position.as_b256())]] {
                    Ok(value) => {
                        let raw: [u8; 32] = value.to_be_bytes();
                        ok_outcome(*index, BlockxStateReadValue::storage(raw.into()))
                    }
                    Err(err) => err_outcome(*index, err.clone()),
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
        batch: &BlockxStateReadBatch,
        mut outcomes: Vec<BlockxStateReadOutcome>,
        block_ctx: &Option<DebankBlockContext>,
    ) -> Vec<BlockxStateReadOutcome> {
        let Some(client) = self.should_try_historical(block_ctx) else {
            return outcomes;
        };
        let failed: Vec<usize> = outcomes
            .iter()
            .enumerate()
            .filter_map(|(pos, outcome)| outcome.error.is_some().then_some(pos))
            .collect();
        if failed.is_empty() {
            return outcomes;
        }
        let retries = failed
            .iter()
            .map(|&pos| historical_item(client, &batch.reads[pos], block_ctx));
        for (&pos, retried) in failed.iter().zip(futures::future::join_all(retries).await) {
            match retried {
                Ok(value) => {
                    outcomes[pos].value = Some(value);
                    outcomes[pos].error = None;
                }
                Err(historical_err) => {
                    let local = outcomes[pos].error.take().expect("failed item has error");
                    outcomes[pos].error = Some(BlockxStateReadError {
                        code: local.code,
                        message: combine_error_message(&local.message, &historical_err),
                    });
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
        batch: &BlockxStateReadBatch,
        local_err: &jsonrpsee::types::ErrorObjectOwned,
        block_ctx: &Option<DebankBlockContext>,
    ) -> Vec<BlockxStateReadOutcome> {
        let client = self
            .should_try_historical(block_ctx)
            .expect("caller checked historical availability");
        let retries = batch
            .reads
            .iter()
            .map(|read| historical_item(client, read, block_ctx));
        batch
            .reads
            .iter()
            .zip(futures::future::join_all(retries).await)
            .map(|(read, retried)| match retried {
                Ok(value) => ok_outcome(read.index(), value),
                Err(historical_err) => err_outcome(
                    read.index(),
                    BlockxStateReadError {
                        code: local_err.code(),
                        message: combine_error_message(local_err.message(), &historical_err),
                    },
                ),
            })
            .collect()
    }
}

async fn historical_item(
    client: &HttpClient,
    read: &BlockxStateRead,
    block_ctx: &Option<DebankBlockContext>,
) -> Result<BlockxStateReadValue, jsonrpsee::core::ClientError> {
    match read {
        BlockxStateRead::AddressCode { address, .. } => client
            .get_address_code(*address, block_ctx.clone())
            .await
            .map(BlockxStateReadValue::code),
        BlockxStateRead::StorageAt {
            address, position, ..
        } => client
            .get_storage_at(*address, position.clone(), block_ctx.clone())
            .await
            .map(BlockxStateReadValue::storage),
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
    async fn state_read_batch(
        &self,
        batch: BlockxStateReadBatch,
    ) -> RpcResult<BlockxStateReadBatchResp> {
        if let Err(err) = validate_batch(&batch) {
            counter!("leafage_state_batch_requests_total", "outcome" => "invalid").increment(1);
            return Err(err);
        }

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
        let this = self.clone();
        let local_batch = batch.clone();
        let local = utils::spawn_blocking_with_cancel(move |_token| {
            let _permit = permit;
            this.blockx_state_read_batch_inner(&local_batch)
        })
        .await
        .map_err(|_| internal_rpc_err("state read batch failed"))?;

        let block_ctx = Some(batch.block_context.clone());
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
        Ok(BlockxStateReadBatchResp { results })
    }
}
