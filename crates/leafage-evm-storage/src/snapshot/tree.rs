use crate::interface::{BlockContext, EvmStorageRead, EvmStorageWrite, StateDB};
use crate::snapshot::error::Error;
use crate::snapshot::layer::{CacheDiskLayer, DiffLayer, LinkedDiffLayer};
use arc_swap::ArcSwap;
use leafage_evm_types::{BlockId, BlockInfo, BlockNumber, BlockStorageDiff, H256, U256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::debug;

const DEFAULT_DIFF_TREE_DEPTH_LIMIT: usize = 128;

// 1GB
const DEFAULT_MEMORY_LIMIT: usize = 1 << 30;

/// SnapshotTree is a tree structure that stores the state of the EVM.
pub struct SnapshotTree<DB> {
    /// latest is a single linked list that stores the latest state of the EVM.
    /// two cases:
    /// 1. bottom disk db (when init).
    /// 2. top diff -> (top-1) diif -> ... -> bottom disk db.
    latest: ArcSwap<LinkedDiffLayer<DB>>,
    /// diff_map stores all the diff layer of the EVM.
    diff_map: RwLock<HashMap<H256, Arc<LinkedDiffLayer<DB>>>>,
}

impl<DB> SnapshotTree<DB> {
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

impl<DB> SnapshotTree<DB>
where
    DB: StateDB
        + EvmStorageWrite<Error = <DB as StateDB>::Error>
        + BlockContext<Error = <DB as StateDB>::Error>,
{
    pub fn new(db: DB) -> Result<Self, <DB as StateDB>::Error> {
        let mut diffs = HashMap::new();
        let info = db.block_info()?;
        let cache_layer = Arc::new(LinkedDiffLayer::CacheDiskLayer(CacheDiskLayer::new(db)));
        diffs.insert(info.hash, cache_layer.clone());
        Ok(Self {
            latest: ArcSwap::new(cache_layer),
            diff_map: RwLock::new(diffs),
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
        block_info: BlockInfo,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error> {
        if let Some(_) = self.diff_map.read().unwrap().get(&block_info.hash) {
            debug!("block {:?} already exists", block_info.hash);
            return Ok(());
        }
        if let Some(parent_layer) = self.diff_map.read().unwrap().get(&block_info.parent_hash) {
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
            self.latest.store(new_diff_layer.clone());
            let bottom_height = new_diff_layer
                .cap_diff_to_db(DEFAULT_DIFF_TREE_DEPTH_LIMIT, DEFAULT_MEMORY_LIMIT)?;
            self.clear_diff_map(bottom_height);
            Ok(())
        } else {
            Err(Error::ParentBlockHashNotFound)
        }
    }
}

impl<DB: BlockContext> BlockContext for SnapshotTree<DB> {
    type Error = Error<DB::Error>;
    fn block_info(&self) -> Result<BlockInfo, Self::Error> {
        self.latest.load().block_info()
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
            BlockId::Hash(hash) => {
                if let Some(layer) = self.diff_map.read().unwrap().get(&hash) {
                    if layer.is_diff_layer() {
                        return Ok(Some(layer.clone()));
                    }
                }
                Ok(None)
            }
            BlockId::Number(number) => match number {
                BlockNumber::Latest | BlockNumber::Pending => Ok(Some(self.latest.load().clone())),
                BlockNumber::Number(num) => {
                    let mut layer = self.latest.load().clone();
                    loop {
                        if layer.is_diff_layer() {
                            let diff_layer = layer.unwrap_diff_layer();
                            if diff_layer.block_info.number == num {
                                return Ok(Some(layer));
                            } else if diff_layer.block_info.number < num {
                                return Ok(None);
                            } else {
                                layer = diff_layer.next.load().clone();
                            }
                        } else {
                            return Ok(None);
                        }
                    }
                }
                _ => unreachable!(),
            },
        }
    }
}
