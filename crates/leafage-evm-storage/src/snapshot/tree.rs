use crate::interface::{BlockContext, EvmStorageRead, EvmStorageWrite, StateDB};
use crate::snapshot::error::Error;
use crate::snapshot::layer::{CacheDiskLayer, DiffLayer, LinkedDiffLayer};
use arc_swap::ArcSwap;
use leafage_evm_types::{Block, BlockId, BlockNumber, BlockStorageDiff, Transaction, H256, U64};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::info;

#[derive(Clone, Debug)]
pub struct Config {
    /// diff_tree_depth_limit is the max depth of the uncommitted diff tree.
    pub diff_tree_depth_limit: usize,
    /// cache_tree_depth_limit is the max depth of the cache committed diff tree.
    pub cache_tree_depth_limit: usize,
}

impl Config {
    pub fn new(diff_tree_depth_limit: usize, cache_tree_depth_limit: usize) -> Self {
        Self {
            diff_tree_depth_limit,
            cache_tree_depth_limit,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            diff_tree_depth_limit: 64,
            cache_tree_depth_limit: 512,
        }
    }
}

/// [`SnapshotTree`] is a tree structure that stores the state of the EVM.
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
    /// config stores the config of the SnapshotTree.
    config: Config,
}

impl<DB> SnapshotTree<DB> {
    /// clear_diff_map removes the diff layer that is lower than bottom_height.
    fn clear_diff_map(&self, bottom_height: U64) {
        if bottom_height.is_zero() {
            return;
        }
        self.hash_diff_map.write().unwrap().retain(|_, v| {
            if let Some(diff_layer) = v.diff_layer() {
                if diff_layer.block_info.number.unwrap() > bottom_height {
                    return true;
                } else {
                    false
                }
            } else {
                true
            }
        });
        self.num_diff_map
            .write()
            .unwrap()
            .retain(|num, v| v.is_cache_layer() || *num > bottom_height.as_u64());
    }
}

impl<DB, E> SnapshotTree<DB>
where
    DB: StateDB<Error = E> + EvmStorageWrite<Error = E> + BlockContext<Error = E>,
{
    pub fn new(db: DB, config: Config) -> Result<Self, E> {
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
            config,
        })
    }
}

impl<DB, E> EvmStorageWrite for SnapshotTree<DB>
where
    DB: EvmStorageWrite<Error = E> + BlockContext<Error = E>,
    E: std::error::Error + Send + Sync + 'static,
{
    type Error = Error<E>;
    /// update_block updates the state of the SnapshotTree.
    ///
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
            info!(target:"storage", "block {:?} already exists", block_info.hash);
            return Ok(());
        }
        let res = self
            .hash_diff_map
            .read()
            .unwrap()
            .get(&block_info.parent_hash)
            .cloned();
        if let Some(parent_layer) = res {
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
                info!(target:"storage", "import reorg block {:?} -> {:?}", block_info.number.unwrap(), latest_block_info.number.unwrap());
                return Ok(());
            }
            self.latest.store(new_diff_layer.clone());
            let bottom_height = new_diff_layer.cap_diff_to_db(
                self.config.diff_tree_depth_limit,
                self.config.cache_tree_depth_limit,
            )?;
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

impl<DB, E> EvmStorageRead for SnapshotTree<DB>
where
    DB: StateDB<Error = E> + BlockContext<Error = E> + Send + Sync,
    E: std::error::Error + Send + Sync + 'static,
{
    type Error = Error<E>;
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
