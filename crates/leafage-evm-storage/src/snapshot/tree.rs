use crate::interface::{BlockContext, EvmStorageRead, EvmStorageWrite, StateDB};
use crate::snapshot::error::Error;
use crate::snapshot::layer::{CacheDiskLayer, DiffLayer, LinkedDiffLayer};
use arc_swap::ArcSwap;
use leafage_evm_types::{Block, BlockId, BlockNumber, BlockStorageDiff, Transaction, H256, U64};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, info};

const DEFAULT_DIFF_TREE_DEPTH_LIMIT: usize = 128;

const DEFAULT_ITEM_NUMS: usize = 1 << 20;

/// SnapshotTree is a tree structure that stores the state of the EVM.
pub struct SnapshotTree<DB> {
    /// latest is a single linked list that stores the latest state of the EVM.
    /// two cases:
    /// 1. bottom disk db (when init).
    /// 2. top diff -> (top-1) diif -> ... -> bottom disk db.
    latest: ArcSwap<LinkedDiffLayer<DB>>,
    /// blockhash -> node, hash_diff_map stores all the diff layer of the EVM.
    hash_diff_map: RwLock<HashMap<H256, Arc<LinkedDiffLayer<DB>>>>,
    /// blocknum-> node, num_diff_map stores all the diff layer of the EVM.
    num_diff_map: RwLock<HashMap<u64, Arc<LinkedDiffLayer<DB>>>>,
}

impl<DB> SnapshotTree<DB> {
    fn clear_diff_map(&self, bottom_height: U64) {
        if bottom_height.is_zero() {
            return;
        }
        let diff_map = self.hash_diff_map.read().unwrap();
        let mut remove_hashes = Vec::new();
        let mut remvoe_nums = Vec::new();
        for (hash, layer) in diff_map.iter() {
            if let Some(layer) = layer.diff_layer() {
                if layer.block_info.number.unwrap() < bottom_height {
                    remove_hashes.push(hash.clone());
                    remvoe_nums.push(layer.block_info.number.unwrap().as_u64());
                }
            }
        }
        let mut diff_map = self.hash_diff_map.write().unwrap();
        for key in remove_hashes {
            diff_map.remove(&key);
        }
        let mut diff_map = self.num_diff_map.write().unwrap();
        for key in remvoe_nums {
            diff_map.remove(&key);
        }
    }
}

impl<DB> SnapshotTree<DB>
where
    DB: StateDB
        + EvmStorageWrite<Error = <DB as StateDB>::Error>
        + BlockContext<Error = <DB as StateDB>::Error>,
{
    pub fn new(db: DB) -> Result<Self, <DB as StateDB>::Error> {
        let mut hash_diffs = HashMap::new();
        let mut num_diffs = HashMap::new();
        let info = db.block_info()?;
        let cache_layer = Arc::new(LinkedDiffLayer::CacheDiskLayer(CacheDiskLayer::new(db)));
        hash_diffs.insert(info.hash.unwrap(), cache_layer.clone());
        num_diffs.insert(info.number.unwrap().as_u64(), cache_layer.clone());
        Ok(Self {
            latest: ArcSwap::new(cache_layer),
            hash_diff_map: RwLock::new(hash_diffs),
            num_diff_map: RwLock::new(num_diffs),
        })
    }
}

impl<DB> EvmStorageWrite for SnapshotTree<DB>
where
    DB: EvmStorageWrite + BlockContext<Error = <DB as EvmStorageWrite>::Error>,
{
    type Error = Error<<DB as EvmStorageWrite>::Error>;
    fn update_block(
        &self,
        block_info: Block<Transaction>,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error> {
        if let Some(_) = self
            .hash_diff_map
            .read()
            .unwrap()
            .get(&block_info.hash.unwrap())
        {
            debug!(target:"storage", "block {:?} already exists", block_info.hash);
            return Ok(());
        }
        if let Some(parent_layer) = self
            .hash_diff_map
            .read()
            .unwrap()
            .get(&block_info.parent_hash)
        {
            let new_diff_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
                block_info.clone(),
                block_diff.clone(),
                parent_layer.clone(),
            )));
            self.hash_diff_map
                .write()
                .unwrap()
                .insert(block_info.hash.unwrap(), new_diff_layer.clone());

            let latest = self.latest.load().clone();
            let latest_block_info = latest.block_info()?;
            // import reorg block
            if block_info.number.unwrap() < latest_block_info.number.unwrap() {
                info!(target:"storage", "reorg block {:?} -> {:?}", block_info.number.unwrap(), latest_block_info.number.unwrap());
                return Ok(());
            }
            self.latest.store(new_diff_layer.clone());
            let bottom_height =
                new_diff_layer.cap_diff_to_db(DEFAULT_DIFF_TREE_DEPTH_LIMIT, DEFAULT_ITEM_NUMS)?;
            self.clear_diff_map(bottom_height);
            Ok(())
        } else {
            Err(Error::ParentBlockHashNotFound)
        }
    }
}

impl<DB: BlockContext> BlockContext for SnapshotTree<DB> {
    type Error = Error<DB::Error>;
    fn block_info(&self) -> Result<Block<Transaction>, Self::Error> {
        self.latest.load().block_info()
    }

    fn block_info_arc(&self) -> Result<Arc<Block<Transaction>>, Self::Error> {
        self.latest.load().block_info_arc()
    }
}

impl<DB> EvmStorageRead for SnapshotTree<DB>
where
    DB: StateDB + BlockContext<Error = <DB as StateDB>::Error> + Send + Sync,
{
    type Error = Error<<DB as StateDB>::Error>;
    type StateDB = Arc<LinkedDiffLayer<DB>>;
    fn state_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDB>, Self::Error> {
        match block_arg {
            BlockId::Hash(hash) => Ok(self.hash_diff_map.read().unwrap().get(&hash).cloned()),
            BlockId::Number(number) => match number {
                BlockNumber::Latest | BlockNumber::Pending => Ok(Some(self.latest.load().clone())),
                BlockNumber::Number(num) => Ok(self
                    .num_diff_map
                    .read()
                    .unwrap()
                    .get(&num.as_u64())
                    .cloned()),
                _ => unreachable!(),
            },
        }
    }
}
