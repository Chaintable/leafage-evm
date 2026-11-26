//! opBNB mainnet as a [`PipelineChain`] for the bundle export.

use super::replay::{decode_block, OpBnbBlock, OpBnbReplayBlock, OpBnbReplayer};
use crate::pipeline::chain::{ChainReplay, PipelineChain, ReplayOptions, ReplayOutput};
use crate::pipeline::changes::LazyState;
use crate::pipeline::genesis::{genesis_output, GenesisOptions};
use alloy::primitives::{Address, B256};
use alloy::rpc::types::Block as RpcBlock;
use anyhow::{anyhow, Result};
use leafage_evm_storage::{EvmStorageRead, EvmStorageWrapper};
use leafage_evm_types::{BlockId, BlockInfo, H256};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

/// The opBNB mainnet genesis alloc (`bnb-chain/op-geth`
/// `core/assets/mainnet/genesis.json`): every account, 2064 predeploys with
/// code, and the original keys of its storage. Values, code and storage are
/// read from the archive state of block 0.
static GENESIS_KEYS: LazyLock<BTreeMap<Address, Vec<B256>>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("mainnet_genesis_keys.json"))
        .expect("valid pinned opBNB genesis keys")
});

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
            parent_state_root: parent.header.state_root,
        })
    }

    fn genesis(
        &self,
        db: &Arc<DB>,
        info: &BlockInfo,
        block: &OpBnbBlock,
        options: ReplayOptions,
    ) -> Result<ReplayOutput> {
        // The genesis block has no transactions: only its header is used.
        let mut genesis = RpcBlock::default();
        genesis.header = block.header.clone();
        let state = archive_state(db.as_ref(), info)?;
        let addresses: Vec<_> = GENESIS_KEYS.keys().copied().collect();
        genesis_output(
            &addresses,
            &state,
            &genesis,
            options,
            // Like the opBNB writer's `GenesisAllocToStateDiff`: one code
            // entry per account with code, the predeploys' shared code too.
            GenesisOptions {
                storage_keys: Some(&GENESIS_KEYS),
                strict: true,
                deduplicate_code: false,
            },
        )
    }
}

pub(crate) struct OpBnbReplay<DB: EvmStorageRead> {
    db: Arc<DB>,
    replayer: OpBnbReplayer<State<DB>>,
    /// Of the last block replayed.
    parent_state_root: B256,
}

impl<DB> ChainReplay for OpBnbReplay<DB>
where
    DB: EvmStorageRead + Send + Sync + 'static,
{
    type Block = OpBnbBlock;

    fn replay(&mut self, info: &BlockInfo, block: OpBnbBlock) -> Result<ReplayOutput> {
        let state_root = block.header.state_root;
        let output = self.replayer.replay(OpBnbReplayBlock {
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
    Ok(EvmStorageWrapper::new(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_keys_cover_the_alloc() {
        assert_eq!(GENESIS_KEYS.len(), 2064);
        assert_eq!(GENESIS_KEYS.values().map(Vec::len).sum::<usize>(), 2079);
        let l1_block: Address = "0x4200000000000000000000000000000000000015"
            .parse()
            .unwrap();
        assert!(GENESIS_KEYS.contains_key(&l1_block));
    }
}
