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
            check_state: options.check_state,
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
    check_state: bool,
}

impl<DB> ChainReplay for ArcReplay<DB>
where
    DB: EvmStorageRead + Send + Sync + 'static,
{
    fn replay(&mut self, info: &BlockInfo, block: FetchedBlock) -> Result<ReplayOutput> {
        let state_root = block.block.header.state_root;
        let output = self.replayer.replay(ArcBlock {
            post_state: self
                .check_state
                .then(|| archive_state(self.db.as_ref(), info))
                .transpose()?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_storage::{BlockContext, StateDB};
    use leafage_evm_types::{AccountInfo, BlockStorageDiff, Bytecode, U256};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, thiserror::Error)]
    #[error("post-state unavailable")]
    struct ReadError;
    impl revm::database_interface::DBErrorMarker for ReadError {}

    #[derive(Clone, Debug)]
    struct TestState;

    impl StateDB for TestState {
        type Error = ReadError;
        fn basic(&self, _: B256) -> Result<Option<AccountInfo>, ReadError> {
            Ok(Some(AccountInfo {
                nonce: 1,
                ..Default::default()
            }))
        }
        fn code_by_hash(&self, _: B256) -> Result<Bytecode, ReadError> {
            Ok(Bytecode::default())
        }
        fn storage(&self, _: B256, _: B256) -> Result<U256, ReadError> {
            Ok(U256::ZERO)
        }
        fn block_hash(&self, _: u64) -> Result<B256, ReadError> {
            Ok(B256::ZERO)
        }
    }

    impl BlockContext for TestState {
        type Error = ReadError;
        fn block_info(&self) -> Result<BlockInfo, ReadError> {
            Err(ReadError)
        }
        fn state_diff(&self) -> Result<BlockStorageDiff, ReadError> {
            Err(ReadError)
        }
    }

    struct TestStorage(AtomicUsize);

    impl EvmStorageRead for TestStorage {
        type Error = ReadError;
        type StateDB = TestState;
        fn state_at(&self, block: BlockId) -> Result<Option<TestState>, ReadError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            if block == BlockId::Hash(B256::repeat_byte(9).into()) {
                return Err(ReadError);
            }
            Ok(Some(TestState))
        }
    }

    #[test]
    fn disabled_state_check_does_not_open_post_state() {
        for check_state in [false, true] {
            let db = Arc::new(TestStorage(AtomicUsize::new(0)));
            let mut parent = BlockInfo::default();
            parent.header.hash = B256::repeat_byte(8);
            let options = ReplayOptions {
                strict: true,
                check_state,
                state_diff: true,
                traces: false,
            };
            let mut replay = ArcPipeline.start(&db, &parent, options).unwrap();
            let mut block: RpcBlock = Default::default();
            block.header.number = 1;
            block.header.hash = B256::repeat_byte(9);
            block.header.gas_limit = 30_000_000;
            block.header.base_fee_per_gas = Some(1);
            block.header.extra_data = 1u64.to_be_bytes().to_vec().into();
            block.header.receipts_root = alloy::consensus::constants::EMPTY_ROOT_HASH;
            block.transactions = alloy::rpc::types::BlockTransactions::Full(vec![]);
            let mut info = BlockInfo::default();
            info.header = block.header.clone();
            let result = replay.replay(
                &info,
                FetchedBlock {
                    block,
                    ommers: vec![],
                },
            );
            if check_state {
                assert!(result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("post-state unavailable"));
                assert_eq!(db.0.load(Ordering::Relaxed), 2);
            } else {
                assert!(result.is_ok());
                assert_eq!(db.0.load(Ordering::Relaxed), 1);
            }
        }
    }
}
