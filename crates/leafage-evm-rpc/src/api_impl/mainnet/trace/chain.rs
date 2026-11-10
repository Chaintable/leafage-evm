//! Ethereum mainnet as a [`PipelineChain`] for the bundle export.

use super::replay::{EthBlock, EthReplayer};
use crate::pipeline::blocks::FetchedBlock;
use crate::pipeline::chain::{ChainReplay, PipelineChain, ReplayOptions, ReplayOutput};
use crate::pipeline::changes::LazyState;
use crate::pipeline::genesis::{genesis_output, GenesisOptions};
use alloy::primitives::B256;
use alloy::rpc::types::Block as RpcBlock;
use anyhow::{anyhow, Result};
use leafage_evm_chains::eth::mainnet::eth_mainnet_genesis_alloc;
use leafage_evm_storage::{EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{BlockId, BlockInfo};
use std::sync::Arc;

pub(crate) struct EthPipeline;

type State<DB> = EvmStorageWrapper<<DB as EvmStorageRead>::StateDB>;

impl<DB> PipelineChain<DB> for EthPipeline
where
    DB: EvmStorageRead + Send + Sync + 'static,
    DB::StateDB: Send + 'static,
{
    type Replay = EthReplay<DB>;

    fn start(
        &self,
        db: &Arc<DB>,
        parent: &BlockInfo,
        options: ReplayOptions,
    ) -> Result<EthReplay<DB>> {
        let state = archive_state(db.as_ref(), parent)?;
        Ok(EthReplay {
            db: db.clone(),
            replayer: EthReplayer::new(state, options),
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
        genesis_output(
            eth_mainnet_genesis_alloc(),
            &state,
            block,
            options,
            GenesisOptions::default(),
        )
    }
}

pub(crate) struct EthReplay<DB: EvmStorageRead> {
    db: Arc<DB>,
    replayer: EthReplayer<State<DB>>,
    /// Of the last block replayed.
    parent_state_root: B256,
}

impl<DB> ChainReplay for EthReplay<DB>
where
    DB: EvmStorageRead + Send + Sync + 'static,
{
    fn replay(&mut self, info: &BlockInfo, block: FetchedBlock) -> Result<ReplayOutput> {
        let state_root = block.block.header.state_root;
        let output = self.replayer.replay(EthBlock {
            post_state: archive_state(self.db.as_ref(), info)?,
            parent_state: Some(Box::new(LazyState::new(
                self.db.clone(),
                info.header.parent_hash,
            ))),
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
