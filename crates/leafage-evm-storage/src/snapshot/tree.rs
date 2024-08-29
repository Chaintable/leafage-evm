use crate::interface::{BlockContext, EvmStorageRead, EvmStorageWrite, StateDB};
use crate::metrics::BLOCK_PRODUCED_TOTAL;
use crate::snapshot::error::Error;
use crate::snapshot::layer::{CacheDiskLayer, DiffLayer, LinkedDiffLayer};
use leafage_evm_types::{Block, BlockId, BlockNumberOrTag, BlockStorageDiff, Transaction, H256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::info;

#[derive(Clone, Debug)]
pub struct Config {
    /// diff_tree_depth_limit is the max depth of the uncommitted diff tree.
    pub diff_tree_depth_limit: usize,
    pub account_cache_size: usize,
    pub storage_cache_size: usize,
    pub code_cache_size: usize,
}

impl Config {
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

impl Default for Config {
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
    config: Config,
}

impl<DB> SnapshotTree<DB> {
    /// clear_diff_map removes the diff layer that is lower than bottom_height.
    fn clear_diff_map(&self, bottom_height: u64) {
        if bottom_height == 0 {
            return;
        }
        self.hash_diff_map.write().unwrap().retain(|_, v| {
            v.is_cache_layer()
                || v.unwrap_diff_layer().block_info.header.number.unwrap() > bottom_height
        });
        self.num_diff_map
            .write()
            .unwrap()
            .retain(|num, v| v.is_cache_layer() || *num > bottom_height);
    }

    pub fn get_config(&self) -> Config {
        self.config.clone()
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
        let cache_layer = Arc::new(LinkedDiffLayer::CacheDiskLayer(CacheDiskLayer::new(
            db,
            config.account_cache_size,
            config.storage_cache_size,
            config.code_cache_size,
        )));
        hash_diffs.insert(info.header.hash.unwrap(), cache_layer.clone());
        num_diffs.insert(info.header.number.unwrap(), cache_layer.clone());
        Ok(Self {
            latest: RwLock::new(cache_layer),
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
    fn update_block(
        &self,
        block_info: Block<Transaction>,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error> {
        if let Some(_) = self
            .hash_diff_map
            .read()
            .unwrap()
            .get(&block_info.header.hash.unwrap())
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
                .insert(block_info.header.hash.unwrap(), new_diff_layer.clone());

            self.num_diff_map
                .write()
                .unwrap()
                .insert(block_info.header.number.unwrap(), new_diff_layer.clone());

            let latest_block_info = self.latest.read().unwrap().block_info()?;
            // import reorg block
            if block_info.header.number.unwrap() < latest_block_info.header.number.unwrap() {
                info!(target:"storage", "import reorg block {:?} -> {:?}", block_info.header.number.unwrap(), latest_block_info.header.number.unwrap());
                return Ok(());
            }
            *self.latest.write().unwrap() = new_diff_layer.clone();
            let bottom_height = new_diff_layer.cap_diff_to_db(self.config.diff_tree_depth_limit)?;
            info!(target:"storage", "clear diff map bottom_height: {:?}", bottom_height);
            self.clear_diff_map(bottom_height);
            BLOCK_PRODUCED_TOTAL.inc();
            Ok(())
        } else {
            Err(Error::ParentBlockHashNotFound)
        }
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
