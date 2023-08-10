use crate::interface::{BlockContext, EvmStorageRead, EvmStorageWrite, StateDB};
use crate::linked_diff::error::Error;
use crate::linked_diff::layer::{CacheLayer, DiffLayer, LinkedDiffLayer};
use arc_swap::ArcSwap;
use leafage_evm_types::{BlockDiff, BlockInfo};
use reth_primitives::{BlockId, H256, U256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::debug;

const DEFAULT_DIFF_TREE_DEPTH_LIMIT: usize = 128;

// 1GB
const DEFAULT_MEMORY_LIMIT: usize = 1 << 30;

/// LinkedDiffTree is a tree structure that stores the state of the EVM.
pub struct LinkedDiffTree<DB> {
    /// latest is a single linked list that stores the latest state of the EVM.
    /// two cases:
    /// 1. bottom disk db (when init).
    /// 2. top diff -> (top-1) diif -> ... -> bottom disk db.
    latest: ArcSwap<LinkedDiffLayer<DB>>,
    /// cache is a single linked list that stores the latest state of the EVM.
    /// two cases:
    /// 1. flatten cache map  -> bottom disk db (init).
    /// 2. top diff -> flatten cache map -> bottom disk db. (more than one diff layer)
    cache: ArcSwap<LinkedDiffLayer<DB>>,
    /// diff_map stores all the diff layer of the EVM.
    diff_map: RwLock<HashMap<H256, Arc<LinkedDiffLayer<DB>>>>,
}

impl<DB> LinkedDiffTree<DB> {
    fn clear_diff_map(&self, bottom_height: U256) {
        if bottom_height == U256::ZERO {
            return;
        }
        let diff_map = self.diff_map.read().unwrap();
        let mut remove_keys = Vec::new();
        for (hash, layer) in diff_map.iter() {
            if layer.is_diff_layer() {
                if layer.unwrap_diff_layer().block_info.number < bottom_height {
                    remove_keys.push(hash.clone());
                }
            }
        }
        let mut diff_map = self.diff_map.write().unwrap();
        for key in remove_keys {
            diff_map.remove(&key);
        }
    }
}

impl<DB> LinkedDiffTree<DB>
where
    DB: StateDB + BlockContext<Error = <DB as StateDB>::Error>,
{
    pub fn new(db: DB) -> Result<Self, <DB as StateDB>::Error> {
        let mut diffs = HashMap::new();
        let info = db.block_info()?;
        let disk_layer = Arc::new(LinkedDiffLayer::DiskLayer(db));
        diffs.insert(info.hash, disk_layer.clone());
        let cache_layer = Arc::new(LinkedDiffLayer::CacheLayer(CacheLayer::new(
            disk_layer.clone(),
        )));
        Ok(Self {
            latest: ArcSwap::new(disk_layer.clone()),
            cache: ArcSwap::new(cache_layer.clone()),
            diff_map: RwLock::new(diffs),
        })
    }
}

impl<DB> EvmStorageWrite for LinkedDiffTree<DB>
where
    DB: EvmStorageWrite + BlockContext<Error = <DB as EvmStorageWrite>::Error>,
{
    type Error = Error<<DB as EvmStorageWrite>::Error>;
    fn update_block(
        &self,
        block_info: BlockInfo,
        block_diff: BlockDiff,
    ) -> Result<(), Self::Error> {
        if let Some(_) = self.diff_map.read().unwrap().get(&block_info.hash) {
            debug!("block {} already exists", block_info.hash);
            return Ok(());
        }
        if let Some(parent_layer) = self.diff_map.read().unwrap().get(&block_info.parent_root) {
            let new_diff_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
                block_info.clone(),
                block_diff.clone(),
                parent_layer.clone(),
            )));
            self.diff_map
                .write()
                .unwrap()
                .insert(block_info.hash, new_diff_layer.clone());

            let latest = self.latest.load().clone();
            let latest_block_info = latest.block_info()?;
            // import reorg front block
            if block_info.number < latest_block_info.number {
                return Ok(());
            }
            if block_info.parent_hash == latest_block_info.hash {
                // normal import
                self.latest.store(new_diff_layer.clone());
                let new_cache_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
                    block_info.clone(),
                    block_diff.clone(),
                    self.cache.load().clone(),
                )));
                self.cache.store(new_cache_layer.clone());
                new_cache_layer.flatten_diff_to_cache_layer();
            } else {
                // reorg import
                self.latest.store(new_diff_layer.clone());
                let new_cache_layer = new_diff_layer.reorg_flatten_diff_to_cache_layer();
                self.cache.store(new_cache_layer);
            }
            let cache_layer = self.cache.load().clone();
            let latest = self.latest.load().clone();
            let bottom_height = latest.cap_diff_to_db(DEFAULT_DIFF_TREE_DEPTH_LIMIT)?;
            cache_layer.cap_cache_diff(DEFAULT_DIFF_TREE_DEPTH_LIMIT, DEFAULT_MEMORY_LIMIT)?;
            self.clear_diff_map(bottom_height);
            Ok(())
        } else {
            Err(Error::ParentBlockHashNotFound)
        }
    }
}

impl<DB: BlockContext> BlockContext for LinkedDiffTree<DB> {
    type Error = Error<DB::Error>;
    fn block_info(&self) -> Result<BlockInfo, Self::Error> {
        self.cache.load().block_info()
    }
}

impl<DB> EvmStorageRead for LinkedDiffTree<DB>
where
    DB: StateDB + BlockContext<Error = <DB as StateDB>::Error>,
{
    type Error = Error<<DB as StateDB>::Error>;
    type StateDB = Arc<LinkedDiffLayer<DB>>;
    fn state_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDB>, Self::Error> {
        match block_arg {
            BlockId::Hash(hash) => {
                if let Some(layer) = self.diff_map.read().unwrap().get(&hash.block_hash) {
                    if layer.is_diff_layer() {
                        return Ok(Some(layer.clone()));
                    }
                }
                Ok(None)
            }
            BlockId::Number(number) => {
                if number.is_latest() || number.is_pending() {
                    return Ok(Some(self.cache.load().clone()));
                }
                if let Some(number) = number.as_number() {
                    let cache = self.cache.load().clone();
                    let block_hash = cache.block_hash(U256::from(number))?;
                    if !block_hash.is_zero() {
                        if let Some(layer) = self.diff_map.read().unwrap().get(&block_hash) {
                            if layer.is_diff_layer() {
                                return Ok(Some(layer.clone()));
                            }
                        }
                    }
                }
                Ok(None)
            }
        }
    }
}
