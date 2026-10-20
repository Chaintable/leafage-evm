//! `trace_debankBlock` on an archive node, for any chain with a
//! [`PipelineChain`] replay.
//!
//! The chain's own node (bsc-x, go-ethereum-x) serves this method by
//! re-executing a block with the pipeline tracer. Here the block is
//! re-executed on the archive state instead, so the pipeline block file
//! (traces, events) can be regenerated for any archived height. Block bodies
//! are not stored in the archive and come from an upstream RPC of the chain
//! (`--trace-block-rpc`).

use super::blocks::fetch_blocks;
use super::chain::{PipelineChain, ReplayOptions};
pub(crate) use super::types::DebankBlockOutput;
use crate::api_impl::core::{Api, ApiBase};
use crate::error::rpc_error_with_code;
use jsonrpsee::core::{async_trait, RpcResult};
use jsonrpsee::http_client::HttpClient;
use jsonrpsee::proc_macros::rpc;
use leafage_evm_storage::{BlockIndex, EvmStorageRead};
use leafage_evm_types::{BlockId, DebankErrorCode};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{info, warn};

#[rpc(server, namespace = "trace")]
pub(crate) trait TraceDebankApi {
    /// Pipeline block file, header and state diff of a block, in the
    /// go-ethereum-x / bsc-x `trace_debankBlock` format.
    #[method(name = "debankBlock")]
    async fn debank_block(
        &self,
        block_id: BlockId,
        options: Option<TraceBlockOptions>,
    ) -> RpcResult<DebankBlockOutput>;
}

/// Optional last parameter of `trace_debankBlock`.
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

pub(crate) struct TraceDebank<C, P> {
    api: Api<C>,
    chain: Arc<P>,
    block_client: HttpClient,
    /// Compare the replay's state changes with the archive.
    check_state: bool,
}

impl<C, P> TraceDebank<C, P> {
    pub(crate) fn new(api: Api<C>, chain: P, block_client: HttpClient, check_state: bool) -> Self {
        Self {
            api,
            chain: Arc::new(chain),
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

#[async_trait]
impl<C, P> TraceDebankApiServer for TraceDebank<C, P>
where
    C: ApiBase,
    C::DB: EvmStorageRead + BlockIndex + Clone + Sync + Send + 'static,
    P: PipelineChain<C::DB>,
{
    async fn debank_block(
        &self,
        block_id: BlockId,
        options: Option<TraceBlockOptions>,
    ) -> RpcResult<DebankBlockOutput> {
        let started = std::time::Instant::now();
        let db = Arc::new(self.api.inner.db().clone());
        let info = db
            .get_block_by_id_arc(block_id)
            .map_err(db_error)?
            .ok_or_else(|| block_not_found(format!("block {block_id:?} is not in the archive")))?;
        let number = info.header.number;
        let parent = if number == 0 {
            None
        } else {
            let id = BlockId::Hash(info.header.parent_hash.into());
            Some(
                db.get_block_by_id_arc(id)
                    .map_err(db_error)?
                    .ok_or_else(|| {
                        block_not_found(format!(
                            "the parent of block {number} is not in the archive"
                        ))
                    })?,
            )
        };
        let block = fetch_blocks(&self.block_client, &[info.header.hash])
            .await
            .map_err(|err| internal_error(format!("fetch block {number} upstream: {err:#}")))?
            .pop()
            .expect("one block fetched");
        let fetched = started.elapsed();

        let permit = match self.api.inner.evm_cfg().exec_limiter.as_ref() {
            Some(limiter) => Some(
                limiter
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(internal_error)?,
            ),
            None => None,
        };
        let options = ReplayOptions {
            strict: true,
            check_state: self.check_state,
            state_diff: options.unwrap_or_default().state_diff,
            traces: true,
        };
        let chain = self.chain.clone();
        // The permit goes with the replay: a request that times out does not
        // stop it.
        let output = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match parent {
                None => chain.genesis(&db, &info, &block.block, options),
                Some(parent) => {
                    let mut replay = chain.start(&db, &parent, options)?;
                    crate::pipeline::chain::ChainReplay::replay(&mut replay, &info, block)
                }
            }
        })
        .await
        .map_err(internal_error)?
        .map_err(|err| {
            warn!(target: "trace", number, "trace_debankBlock failed: {err:#}");
            rpc_error_with_code(DebankErrorCode::EvmFailed as i32, format!("{err:#}"))
        })?;

        info!(
            target: "trace",
            number,
            txs = output.block_file.txs.len(),
            total_ms = started.elapsed().as_millis() as u64,
            fetch_ms = fetched.as_millis() as u64,
            execute_ms = output.timings.execute.as_millis() as u64,
            verify_ms = output.timings.verify.as_millis() as u64,
            diff_ms = output.timings.diff.as_millis() as u64,
            "trace_debankBlock"
        );
        let validation_hash = output.block_file.validation_hash();
        Ok(DebankBlockOutput {
            block_file: output.block_file,
            header: output.header,
            state_diff: output.state_diff,
            validation_hash,
        })
    }
}
