//! Arc mainnet as a [`PipelineChain`] for the bundle export.

use super::genesis::genesis_output;
use super::replay::{ArcBlock, ArcReplayer};
use crate::pipeline::blocks::FetchedBlock;
use crate::pipeline::chain::{ChainReplay, PipelineChain, ReplayOptions, ReplayOutput};
use alloy::primitives::B256;
use alloy::rpc::types::Block as RpcBlock;
use anyhow::{anyhow, Result};
use leafage_evm_storage::{EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{BlockId, BlockInfo};
use std::sync::Arc;

pub(crate) struct ArcPipeline;

type State<DB> = EvmStorageWrapper<<DB as EvmStorageRead>::StateDB>;

impl<DB> PipelineChain<DB> for ArcPipeline
where
    DB: EvmStorageRead + Send + Sync + 'static,
    DB::StateDB: Send + 'static,
{
    type Replay = ArcReplay<DB>;

    fn start(
        &self,
        db: &Arc<DB>,
        parent: &BlockInfo,
        options: ReplayOptions,
    ) -> Result<ArcReplay<DB>> {
        let state = archive_state(db.as_ref(), parent)?;
        Ok(ArcReplay {
            db: db.clone(),
            replayer: ArcReplayer::new(state, options),
            parent_state_root: parent.header.state_root,
        })
    }

    fn genesis(
        &self,
        db: &Arc<DB>,
        info: &BlockInfo,
        block: &RpcBlock,
        options: ReplayOptions,
    ) -> Result<ReplayOutput> {
        let state = archive_state(db.as_ref(), info)?;
        genesis_output(&state, block, options)
    }
}

pub(crate) struct ArcReplay<DB: EvmStorageRead> {
    db: Arc<DB>,
    replayer: ArcReplayer<State<DB>>,
    /// Of the last block replayed.
    parent_state_root: B256,
}

impl<DB> ChainReplay for ArcReplay<DB>
where
    DB: EvmStorageRead + Send + Sync + 'static,
{
    fn replay(&mut self, info: &BlockInfo, block: FetchedBlock) -> Result<ReplayOutput> {
        let state_root = block.block.header.state_root;
        let output = self.replayer.replay(ArcBlock {
            post_state: archive_state(self.db.as_ref(), info)?,
            parent_state_root: self.parent_state_root,
            block,
        })?;
        self.parent_state_root = state_root;
        Ok(output)
    }

    fn rebase(&mut self, info: &BlockInfo) -> Result<()> {
        self.replayer.rebase(archive_state(self.db.as_ref(), info)?);
        Ok(())
    }
}

/// The archive state after `info`: no OVM, slot keys hashed as they are.
fn archive_state<DB>(db: &DB, info: &BlockInfo) -> Result<State<DB>>
where
    DB: EvmStorageRead,
{
    let number = info.header.number;
    let state = db
        .state_at(BlockId::Hash(info.header.hash.into()))
        .map_err(|err| anyhow!("state of block {number}: {err}"))?
        .ok_or_else(|| anyhow!("the state of block {number} is not in the archive"))?;
    Ok(EvmStorageWrapper {
        db: state,
        ovm_address: None,
        normalize_state_key: false,
    })
}
