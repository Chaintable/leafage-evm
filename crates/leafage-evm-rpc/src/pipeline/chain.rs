//! What a chain provides to regenerate its pipeline outputs: the replay of
//! consecutive blocks on its archive state.

use super::types::{BlockFile, PipelineHeader};
use alloy::primitives::Bytes;
use alloy::rpc::types::Block as RpcBlock;
use anyhow::Result;
use leafage_evm_types::BlockInfo;
use std::str::FromStr;
use std::sync::Arc;

/// The pipeline outputs of one replayed block.
pub(crate) struct ReplayOutput {
    pub block_file: BlockFile,
    pub header: PipelineHeader,
    /// RLP of the `BlockStorageDiff` produced by the replay; empty when the
    /// state root did not change, `None` when not asked for.
    pub state_diff: Option<Bytes>,
    pub timings: ReplayTimings,
}

/// Where a replay spent its time.
#[derive(Debug, Default)]
pub(crate) struct ReplayTimings {
    pub execute: std::time::Duration,
    pub verify: std::time::Duration,
    pub diff: std::time::Duration,
}

/// What a replay checks and produces besides the block files.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReplayOptions {
    /// Fail on a failed check instead of logging it.
    pub strict: bool,
    /// Compare the replay's state changes with the archive (slow).
    pub check_state: bool,
    /// Produce the block's state diff.
    pub state_diff: bool,
    /// Trace the transactions for the traces and events of the block file;
    /// without, its `traces`, `events` and `storage_contracts` stay empty.
    pub traces: bool,
}

/// A chain whose blocks can be replayed on an archive `DB` into pipeline
/// outputs.
pub(crate) trait PipelineChain<DB>: Send + Sync + 'static {
    type Replay: ChainReplay + Send + 'static;

    /// Start replaying after block `parent`, on its archive state.
    fn start(
        &self,
        db: &Arc<DB>,
        parent: &BlockInfo,
        options: ReplayOptions,
    ) -> Result<Self::Replay>;
}

/// The replay of consecutive blocks. The state it builds carries over from
/// one block to the next, so a key is read from the archive only the first
/// time the replay touches it.
pub(crate) trait ChainReplay {
    /// Replay the block after the last one; `info` is its archive entry.
    fn replay(&mut self, info: &BlockInfo, block: RpcBlock) -> Result<ReplayOutput>;

    /// Go on from the archive state after `info`, the last block replayed,
    /// dropping the state built so far to bound its memory.
    fn rebase(&mut self, info: &BlockInfo) -> Result<()>;

    /// Running totals worth logging, e.g. of archive reads.
    fn counters(&self) -> Vec<(&'static str, u64)> {
        Vec::new()
    }
}

/// The chains [`super::export_bundles`] supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    /// BSC mainnet.
    Bsc,
}

impl Chain {
    pub const ALL: [Chain; 1] = [Chain::Bsc];

    pub fn name(self) -> &'static str {
        match self {
            Chain::Bsc => "bsc",
        }
    }

    /// Chain ID in the pipeline's S3 keys.
    pub fn chain_id(self) -> u64 {
        match self {
            Chain::Bsc => 56,
        }
    }
}

impl std::fmt::Display for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Chain {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Chain::ALL
            .into_iter()
            .find(|chain| chain.name() == s.trim().to_ascii_lowercase())
            .ok_or_else(|| {
                let names: Vec<_> = Chain::ALL.iter().map(|chain| chain.name()).collect();
                anyhow::anyhow!(
                    "unsupported chain {s:?}, expected one of {}",
                    names.join(", ")
                )
            })
    }
}
