//! opBNB mainnet as a [`PipelineChain`] for the bundle export.

use super::replay::{decode_block, OpBnbBlock, OpBnbReplayBlock, OpBnbReplayer};
use crate::pipeline::chain::{ChainReplay, PipelineChain, ReplayOptions, ReplayOutput};
use anyhow::{anyhow, bail, Result};
use leafage_evm_storage::{EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{BlockId, BlockInfo, H256};
use std::sync::Arc;

pub(crate) struct OpBnbPipeline;

type State<DB> = EvmStorageWrapper<<DB as EvmStorageRead>::StateDB>;

impl<DB> PipelineChain<DB> for OpBnbPipeline
where
    DB: EvmStorageRead + Send + Sync + 'static,
    DB::StateDB: Send + 'static,
{
    type Block = OpBnbBlock;
    type Replay = OpBnbReplay<DB>;

    fn decode_raw(&self, hash: H256, raw: &[u8]) -> Result<OpBnbBlock> {
        decode_block(hash, raw)
    }

    fn start(
        &self,
        db: &Arc<DB>,
        parent: &BlockInfo,
        options: ReplayOptions,
    ) -> Result<OpBnbReplay<DB>> {
        let state = archive_state(db.as_ref(), parent)?;
        Ok(OpBnbReplay {
            db: db.clone(),
            replayer: OpBnbReplayer::new(state, options),
        })
    }

    fn genesis(
        &self,
        _db: &Arc<DB>,
        _info: &BlockInfo,
        _block: &OpBnbBlock,
        _options: ReplayOptions,
    ) -> Result<ReplayOutput> {
        bail!("the opBNB genesis block is not exported (its block file is already right)")
    }
}

pub(crate) struct OpBnbReplay<DB: EvmStorageRead> {
    db: Arc<DB>,
    replayer: OpBnbReplayer<State<DB>>,
}

impl<DB> ChainReplay for OpBnbReplay<DB>
where
    DB: EvmStorageRead + Send + Sync + 'static,
{
    type Block = OpBnbBlock;

    fn replay(&mut self, info: &BlockInfo, block: OpBnbBlock) -> Result<ReplayOutput> {
        self.replayer.replay(OpBnbReplayBlock {
            post_state: archive_state(self.db.as_ref(), info)?,
            block,
        })
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
    Ok(EvmStorageWrapper::new(state))
}
