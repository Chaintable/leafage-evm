use crate::interface::{BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrite, StateDB};
use crate::metrics::BLOCK_METRICS;
use crate::snapshot::error::Error;
use crate::snapshot::layer::{CacheDiskLayer, DiffLayer, LinkedDiffLayer};
use leafage_evm_types::{Block, BlockId, BlockNumberOrTag, BlockStorageDiff, Transaction, H256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::{debug, info};

#[derive(Clone, Debug)]
pub struct SnapshotTreeConfig {
    /// diff_tree_depth_limit is the max depth of the uncommitted diff tree.
    pub diff_tree_depth_limit: usize,
    pub account_cache_size: usize,
    pub storage_cache_size: usize,
    pub code_cache_size: usize,
}

impl SnapshotTreeConfig {
    pub fn new(
        diff_tree_depth_limit: usize,
        account_cache_size: usize,
        storage_cache_size: usize,
        code_cache_size: usize,
    ) -> Self {
        Self {
            diff_tree_depth_limit,
            account_cache_size,
            storage_cache_size,
            code_cache_size,
        }
    }
}

impl Default for SnapshotTreeConfig {
    fn default() -> Self {
        Self {
            diff_tree_depth_limit: 64,
            account_cache_size: 100000,
            storage_cache_size: 3000000,
            code_cache_size: 100000,
        }
    }
}

/// [`SnapshotTree`] is a tree structure that stores the state of the EVM.
pub struct SnapshotTree<DB> {
    /// latest is a single linked list that stores the latest state of the EVM.
    /// two cases:
    /// 1. bottom disk db (when init).
    /// 2. top diff -> (top-1) diif -> ... -> bottom disk db.
    latest: RwLock<Arc<LinkedDiffLayer<DB>>>,
    /// blockhash -> node, hash_diff_map stores all the diff layer of the EVM.
    hash_diff_map: RwLock<HashMap<H256, Arc<LinkedDiffLayer<DB>>>>,
    /// blocknum-> node, num_diff_map stores all the diff layer of the EVM.
    num_diff_map: RwLock<HashMap<u64, Arc<LinkedDiffLayer<DB>>>>,
    /// config stores the config of the SnapshotTree.
    config: SnapshotTreeConfig,
    /// disk_layer is the bottom layer of the SnapshotTree.
    disk_layer: Arc<LinkedDiffLayer<DB>>,
}

impl<DB> SnapshotTree<DB> {
    /// clear_diff_map removes the diff layer that is lower than bottom_height.
    fn clear_diff_map(&self, bottom_height: u64) {
        if bottom_height == 0 {
            return;
        }
        self.hash_diff_map
            .write()
            .unwrap()
            .retain(|_, v| v.unwrap_diff_layer().block_info.header.number > bottom_height);
        self.num_diff_map
            .write()
            .unwrap()
            .retain(|num, _| *num > bottom_height);
    }

    pub fn get_config(&self) -> SnapshotTreeConfig {
        self.config.clone()
    }

    pub fn get_disk_layer(&self) -> Arc<LinkedDiffLayer<DB>> {
        self.disk_layer.clone()
    }
}

impl<DB, E> SnapshotTree<DB>
where
    DB: StateDB<Error = E> + EvmStorageWrite<Error = E> + BlockContext<Error = E>,
{
    pub fn new(db: DB, config: SnapshotTreeConfig) -> Result<Self, E> {
        let mut hash_diffs = HashMap::new();
        let mut num_diffs = HashMap::new();
        let info = db.block_info()?;
        let cache_layer = Arc::new(LinkedDiffLayer::CacheDiskLayer(CacheDiskLayer::new(
            db,
            config.account_cache_size,
            config.storage_cache_size,
            config.code_cache_size,
        )));
        let bottom_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
            info.clone(),
            BlockStorageDiff::default(),
            cache_layer.clone(),
        )));
        info!(target:"storage", "init block info: {:?}", info);
        hash_diffs.insert(info.header.hash, bottom_layer.clone());
        num_diffs.insert(info.header.number, bottom_layer.clone());
        Ok(Self {
            latest: RwLock::new(cache_layer.clone()),
            hash_diff_map: RwLock::new(hash_diffs),
            num_diff_map: RwLock::new(num_diffs),
            disk_layer: cache_layer,
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
    fn update_block(
        &self,
        block_info: Block<Transaction>,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error> {
        if let Some(_) = self
            .hash_diff_map
            .read()
            .unwrap()
            .get(&block_info.header.hash)
        {
            info!(target:"storage", "block {:?} already exists", block_info.header.hash);
            return Ok(());
        }
        let res = self
            .hash_diff_map
            .read()
            .unwrap()
            .get(&block_info.header.parent_hash)
            .cloned();
        if let Some(parent_layer) = res {
            let new_diff_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
                block_info.clone(),
                block_diff,
                parent_layer.clone(),
            )));
            self.hash_diff_map
                .write()
                .unwrap()
                .insert(block_info.header.hash, new_diff_layer.clone());

            self.num_diff_map
                .write()
                .unwrap()
                .insert(block_info.header.number, new_diff_layer.clone());

            let latest_block_info = self.latest.read().unwrap().block_info()?;
            // import reorg block
            if block_info.header.number < latest_block_info.header.number {
                info!(target:"storage", "import reorg block {:?} -> {:?}", block_info.header.number, latest_block_info.header.number);
                return Ok(());
            }
            BLOCK_METRICS.block_num.set(block_info.header.number as f64);
            BLOCK_METRICS
                .block_time
                .set(block_info.header.timestamp as f64);
            *self.latest.write().unwrap() = new_diff_layer.clone();
            let bottom_height = new_diff_layer.cap_diff_to_db(self.config.diff_tree_depth_limit)?;
            debug!(target:"storage", "clear diff map bottom_height: {:?}", bottom_height);
            self.clear_diff_map(bottom_height);
            Ok(())
        } else {
            Err(Error::ParentBlockHashNotFound(
                block_info.header.hash,
                block_info.header.number,
                block_info.header.parent_hash,
            ))
        }
    }

    fn last_committed_block(&self) -> Result<Option<Block<Transaction>>, Self::Error> {
        self.disk_layer
            .block_info_arc()
            .map(|b| Some(b.as_ref().clone()))
    }
}

impl<DB: BlockContext> BlockContext for SnapshotTree<DB> {
    type Error = Error<DB::Error>;
    fn block_info(&self) -> Result<Block<Transaction>, Self::Error> {
        self.latest.read().unwrap().block_info()
    }

    fn block_info_arc(&self) -> Result<Arc<Block<Transaction>>, Self::Error> {
        self.latest.read().unwrap().block_info_arc()
    }

    fn state_diff(&self) -> Result<BlockStorageDiff, Self::Error> {
        self.latest.read().unwrap().state_diff()
    }

    fn state_diff_arc(&self) -> Result<Arc<BlockStorageDiff>, Self::Error> {
        self.latest.read().unwrap().state_diff_arc()
    }
}

impl<DB: BlockContext> BlockIndex for SnapshotTree<DB> {
    type Error = Error<DB::Error>;

    fn get_block_by_id_arc(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Arc<Block<Transaction>>>, Self::Error> {
        match block_id {
            BlockId::Hash(hash) => {
                if let Some(res) = self.hash_diff_map.read().unwrap().get(&hash.block_hash) {
                    let block = res.block_info_arc()?;
                    return Ok(Some(block));
                }
                Ok(None)
            }
            BlockId::Number(number) => match number {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                    Ok(Some(self.latest.read().unwrap().block_info_arc()?))
                }
                BlockNumberOrTag::Number(num) => {
                    if let Some(res) = self.num_diff_map.read().unwrap().get(&num) {
                        let block = res.block_info_arc()?;
                        return Ok(Some(block));
                    }
                    Ok(None)
                }
                _ => Ok(None),
            },
        }
    }
}

impl<DB, E> EvmStorageRead for SnapshotTree<DB>
where
    DB: StateDB<Error = E> + BlockContext<Error = E> + Send + Sync + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    type Error = Error<E>;
    type StateDB = Arc<LinkedDiffLayer<DB>>;
    fn state_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDB>, Self::Error> {
        match block_arg {
            BlockId::Hash(hash) => Ok(self
                .hash_diff_map
                .read()
                .unwrap()
                .get(&hash.block_hash)
                .cloned()),
            BlockId::Number(number) => match number {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                    Ok(Some(self.latest.read().unwrap().clone()))
                }
                BlockNumberOrTag::Number(num) => {
                    Ok(self.num_diff_map.read().unwrap().get(&num).cloned())
                }
                _ => Err(Error::UnsupportedBlockId(BlockId::Number(number))),
            },
        }
    }
}
