//! `trace_debankBlock` for BSC mainnet on an archive node.
//!
//! bsc-x serves this method by re-executing a block with the pipeline
//! tracer. Here the block is re-executed on the archive state instead, so the
//! pipeline block file (traces, events) can be regenerated for any archived
//! height. Transaction bodies are not stored in the archive and come from an
//! upstream BSC RPC (`--trace-block-rpc`).

mod chain;
mod patches;
mod recreated;
mod replay;

#[cfg(test)]
mod live_test;

use crate::api_impl::core::{Api, ApiBase};
use crate::error::rpc_error_with_code;
use crate::pipeline::blocks::fetch_blocks;
use crate::pipeline::chain::ReplayOptions;
pub(crate) use crate::pipeline::types::DebankBlockOutput;
use alloy::primitives::U64;
pub(crate) use chain::BscPipeline;
use jsonrpsee::core::{async_trait, RpcResult};
use jsonrpsee::http_client::HttpClient;
use jsonrpsee::proc_macros::rpc;
use leafage_evm_storage::{BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{BlockId, BlockNumberOrTag, DebankErrorCode};
use recreated::{FilterStats, RecreatedStorageFilter};
use replay::{replay_range, RangeBlock};
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

#[rpc(server, namespace = "trace")]
pub(crate) trait BscTraceApi {
    /// Pipeline block file, header and state diff of a block, in the bsc-x
    /// `trace_debankBlock` format.
    #[method(name = "debankBlock")]
    async fn debank_block(
        &self,
        block_id: BlockId,
        options: Option<TraceBlockOptions>,
    ) -> RpcResult<DebankBlockOutput>;

    /// [`Self::debank_block`] for the blocks `from..=to` (at most
    /// [`MAX_RANGE`]). Replaying them in one go reads each piece of state from
    /// the archive once for the whole range.
    #[method(name = "debankBlocks")]
    async fn debank_blocks(
        &self,
        from: U64,
        to: U64,
        options: Option<TraceBlockOptions>,
    ) -> RpcResult<Vec<DebankBlockOutput>>;
}

/// Optional last parameter of the `trace_debankBlock(s)` methods.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct TraceBlockOptions {
    /// Produce `state_diff`; turning it off leaves the field out of the
    /// response, for callers that only want the block file.
    pub state_diff: bool,
}

impl Default for TraceBlockOptions {
    fn default() -> Self {
        Self { state_diff: true }
    }
}

/// Most blocks one `trace_debankBlocks` call replays.
pub(crate) const MAX_RANGE: u64 = 100;

pub(crate) struct BscTrace<C> {
    api: Api<C>,
    block_client: HttpClient,
    /// Compare the replay's state changes with the archive (see `verify`).
    check_state: bool,
}

impl<C> BscTrace<C> {
    pub(crate) fn new(api: Api<C>, block_client: HttpClient, check_state: bool) -> Self {
        Self {
            api,
            block_client,
            check_state,
        }
    }
}

fn db_error(err: impl std::fmt::Display) -> jsonrpsee::types::ErrorObjectOwned {
    rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, err.to_string())
}

fn block_not_found(msg: String) -> jsonrpsee::types::ErrorObjectOwned {
    rpc_error_with_code(DebankErrorCode::BlockNotFound as i32, msg)
}

fn internal_error(msg: impl std::fmt::Display) -> jsonrpsee::types::ErrorObjectOwned {
    rpc_error_with_code(DebankErrorCode::InternalError as i32, msg.to_string())
}

fn invalid_params(msg: impl std::fmt::Display) -> jsonrpsee::types::ErrorObjectOwned {
    rpc_error_with_code(DebankErrorCode::InvalidParams as i32, msg.to_string())
}

#[async_trait]
impl<C> BscTraceApiServer for BscTrace<C>
where
    C: ApiBase,
    C::DB: EvmStorageRead + BlockIndex + Sync + Send + 'static,
{
    async fn debank_block(
        &self,
        block_id: BlockId,
        options: Option<TraceBlockOptions>,
    ) -> RpcResult<DebankBlockOutput> {
        let block = self
            .api
            .inner
            .db()
            .get_block_by_id_arc(block_id)
            .map_err(db_error)?
            .ok_or_else(|| block_not_found(format!("block {block_id:?} is not in the archive")))?;
        let number = block.header.number;
        let mut outputs = self
            .trace_range(number, number, options.unwrap_or_default())
            .await?;
        Ok(outputs.pop().expect("one block traced"))
    }

    async fn debank_blocks(
        &self,
        from: U64,
        to: U64,
        options: Option<TraceBlockOptions>,
    ) -> RpcResult<Vec<DebankBlockOutput>> {
        let (from, to) = (from.to::<u64>(), to.to::<u64>());
        if to < from || to - from >= MAX_RANGE {
            return Err(invalid_params(format!(
                "expected from <= to and at most {MAX_RANGE} blocks, got {from}..={to}"
            )));
        }
        self.trace_range(from, to, options.unwrap_or_default())
            .await
    }
}

impl<C> BscTrace<C>
where
    C: ApiBase,
    C::DB: EvmStorageRead + BlockIndex + Sync + Send + 'static,
{
    async fn trace_range(
        &self,
        from: u64,
        to: u64,
        options: TraceBlockOptions,
    ) -> RpcResult<Vec<DebankBlockOutput>> {
        if from == 0 {
            return Err(invalid_params("the genesis block is not supported"));
        }
        let started = std::time::Instant::now();
        let db = self.api.inner.db();
        let mut infos = Vec::with_capacity((to - from + 1) as usize);
        for number in from..=to {
            let id = BlockId::Number(BlockNumberOrTag::Number(number));
            let info = db
                .get_block_by_id_arc(id)
                .map_err(db_error)?
                .ok_or_else(|| block_not_found(format!("block {number} is not in the archive")))?;
            infos.push(info);
        }
        let state_at = |hash: leafage_evm_types::H256, what: &str| {
            db.state_at(BlockId::Hash(hash.into()))
                .map_err(db_error)?
                .ok_or_else(|| block_not_found(format!("{what} is not in the archive")))
        };
        let parent_state = state_at(infos[0].header.parent_hash, "the parent of the first block")?;
        let parent = parent_state.block_info_arc().map_err(db_error)?;
        let mut post_states = Vec::with_capacity(infos.len());
        for info in &infos {
            let number = info.header.number;
            post_states.push(state_at(
                info.header.hash,
                &format!("the state of block {number}"),
            )?);
        }

        let hashes: Vec<_> = infos.iter().map(|info| info.header.hash).collect();
        let rpc_blocks = fetch_blocks(&self.block_client, &hashes)
            .await
            .map_err(|err| {
                internal_error(format!("fetch blocks {from}..={to} upstream: {err:#}"))
            })?;
        let fetched = started.elapsed();

        let _permit = match self.api.inner.evm_cfg().exec_limiter.as_ref() {
            Some(limiter) => Some(
                limiter
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(internal_error)?,
            ),
            None => None,
        };
        let (parent_stats, post_stats) = (Arc::<FilterStats>::default(), Arc::default());
        let wrap = |db, height, stats: &Arc<FilterStats>| {
            RecreatedStorageFilter::new(
                EvmStorageWrapper {
                    db,
                    ovm_address: None,
                    normalize_state_key: false,
                },
                height,
                stats.clone(),
            )
        };
        let mut blocks = Vec::with_capacity(infos.len());
        let (mut parent_timestamp, mut parent_state_root) =
            (parent.header.timestamp, parent.header.state_root);
        for (post_state, block) in post_states.into_iter().zip(rpc_blocks) {
            let (timestamp, state_root) = (block.header.timestamp, block.header.state_root);
            blocks.push(RangeBlock {
                post_state: wrap(post_state, block.header.number, &post_stats),
                parent_timestamp,
                parent_state_root,
                block,
            });
            (parent_timestamp, parent_state_root) = (timestamp, state_root);
        }
        let parent_state = wrap(parent_state, from - 1, &parent_stats);
        let options = ReplayOptions {
            strict: true,
            check_state: self.check_state,
            state_diff: options.state_diff,
            traces: true,
        };
        let outputs =
            tokio::task::spawn_blocking(move || replay_range(parent_state, blocks, options))
                .await
                .map_err(internal_error)?
                .map_err(|err| {
                    warn!(target: "trace", from, to, "trace_debankBlock failed: {err:#}");
                    rpc_error_with_code(DebankErrorCode::EvmFailed as i32, format!("{err:#}"))
                })?;

        let load = |stats: &FilterStats, counter: fn(&FilterStats) -> &AtomicU64| {
            counter(stats).load(Ordering::Relaxed)
        };
        let sum = |part: fn(&crate::pipeline::chain::ReplayTimings) -> std::time::Duration| {
            outputs
                .iter()
                .map(|output| part(&output.timings))
                .sum::<std::time::Duration>()
        };
        info!(
            target: "trace",
            from,
            to,
            txs = outputs.iter().map(|output| output.block_file.txs.len()).sum::<usize>(),
            total_ms = started.elapsed().as_millis() as u64,
            fetch_ms = fetched.as_millis() as u64,
            execute_ms = sum(|t| t.execute).as_millis() as u64,
            verify_ms = sum(|t| t.verify).as_millis() as u64,
            diff_ms = sum(|t| t.diff).as_millis() as u64,
            parent_slots = load(&parent_stats, |s| &s.slots),
            parent_accounts_checked = load(&parent_stats, |s| &s.accounts),
            parent_zeroed = load(&parent_stats, |s| &s.zeroed),
            post_slots = load(&post_stats, |s| &s.slots),
            post_accounts_checked = load(&post_stats, |s| &s.accounts),
            "trace_debankBlock"
        );
        Ok(outputs
            .into_iter()
            .map(|output| {
                let validation_hash = output.block_file.validation_hash();
                DebankBlockOutput {
                    block_file: output.block_file,
                    header: output.header,
                    state_diff: output.state_diff,
                    validation_hash,
                }
            })
            .collect())
    }
}
