//! BSC mainnet as a [`PipelineChain`] for the bundle export.

use super::recreated::{FilterStats, RecreatedStorageFilter};
use super::replay::{RangeBlock, RangeReplayer};
use crate::pipeline::chain::{ChainReplay, PipelineChain, ReplayOptions, ReplayOutput};
use crate::pipeline::genesis::genesis_output;
use alloy::primitives::B256;
use alloy::rpc::types::Block as RpcBlock;
use anyhow::{anyhow, Result};
use leafage_evm_chains::bsc::mainnet::BSC_MAINNET_GENESIS_ALLOC;
use leafage_evm_storage::{EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{BlockId, BlockInfo};
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub(crate) struct BscPipeline;

type Filtered<DB> = RecreatedStorageFilter<<DB as EvmStorageRead>::StateDB>;

impl<DB> PipelineChain<DB> for BscPipeline
where
    DB: EvmStorageRead + Send + Sync + 'static,
    DB::StateDB: Send + 'static,
{
    type Replay = BscReplay<DB>;

    fn start(
        &self,
        db: &Arc<DB>,
        parent: &BlockInfo,
        options: ReplayOptions,
    ) -> Result<BscReplay<DB>> {
        let stats = Arc::new(FilterStats::default());
        let state = filtered_state(db.as_ref(), parent, &stats)?;
        Ok(BscReplay {
            db: db.clone(),
            replayer: RangeReplayer::new(state, options),
            parent_timestamp: parent.header.timestamp,
            parent_state_root: parent.header.state_root,
            stats,
        })
    }

    fn genesis(
        &self,
        db: &Arc<DB>,
        info: &BlockInfo,
        block: &RpcBlock,
        options: ReplayOptions,
    ) -> Result<ReplayOutput> {
        let state = filtered_state(db.as_ref(), info, &Arc::default())?;
        genesis_output(&BSC_MAINNET_GENESIS_ALLOC, &state, block, options)
    }
}

pub(crate) struct BscReplay<DB: EvmStorageRead> {
    db: Arc<DB>,
    replayer: RangeReplayer<Filtered<DB>>,
    /// Of the last block replayed.
    parent_timestamp: u64,
    parent_state_root: B256,
    stats: Arc<FilterStats>,
}

impl<DB> ChainReplay for BscReplay<DB>
where
    DB: EvmStorageRead,
{
    fn replay(&mut self, info: &BlockInfo, block: RpcBlock) -> Result<ReplayOutput> {
        let (timestamp, state_root) = (block.header.timestamp, block.header.state_root);
        let output = self.replayer.replay(RangeBlock {
            post_state: filtered_state(self.db.as_ref(), info, &self.stats)?,
            parent_timestamp: self.parent_timestamp,
            parent_state_root: self.parent_state_root,
            block,
        })?;
        (self.parent_timestamp, self.parent_state_root) = (timestamp, state_root);
        Ok(output)
    }

    fn rebase(&mut self, info: &BlockInfo) -> Result<()> {
        self.replayer
            .rebase(filtered_state(self.db.as_ref(), info, &self.stats)?);
        Ok(())
    }

    fn counters(&self) -> Vec<(&'static str, u64)> {
        vec![
            (
                "accounts_checked",
                self.stats.accounts.load(Ordering::Relaxed),
            ),
            ("zeroed", self.stats.zeroed.load(Ordering::Relaxed)),
        ]
    }
}

/// The archive state after `info`, read through the recreated-storage filter.
fn filtered_state<DB>(db: &DB, info: &BlockInfo, stats: &Arc<FilterStats>) -> Result<Filtered<DB>>
where
    DB: EvmStorageRead,
{
    let number = info.header.number;
    let state = db
        .state_at(BlockId::Hash(info.header.hash.into()))
        .map_err(|err| anyhow!("state of block {number}: {err}"))?
        .ok_or_else(|| anyhow!("the state of block {number} is not in the archive"))?;
    Ok(RecreatedStorageFilter::new(
        EvmStorageWrapper {
            db: state,
            ovm_address: None,
            normalize_state_key: false,
        },
        number,
        stats.clone(),
    ))
}
