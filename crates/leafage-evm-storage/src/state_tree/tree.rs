use crate::db::{StateDBProvider, StateDBWrapper};
use crate::db_impl::StorageError;
use crate::interface::{BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrite};
use crate::metrics::BLOCK_METRICS;
use crate::state_tree::error::Error;
use crate::state_tree::layer::{
    CacheDiskLayer, DiffLayer, FlattenedLayerView, HybridStateDB, LinkedDiffLayer,
};
use arc_swap::ArcSwap;
use leafage_evm_types::{BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff, H256};
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use tracing::{debug, info};

#[derive(Clone, Debug)]
pub struct StateTreeConfig {
    /// diff_tree_depth_limit is the max depth of the uncommitted diff tree.
    pub diff_tree_depth_limit: usize,
    pub account_cache_size: usize,
    pub storage_cache_size: usize,
    pub code_cache_size: usize,
    /// Whether the bottom CacheDiskLayer's moka caches are active. Set
    /// false for backends with per-handle snapshot semantics (MDBX) so
    /// each handle reads directly from its own ro_txn — the shared
    /// cache would otherwise blur snapshot boundaries.
    pub enable_cache: bool,
}

impl StateTreeConfig {
    pub fn new(
        diff_tree_depth_limit: usize,
        account_cache_size: usize,
        storage_cache_size: usize,
        code_cache_size: usize,
        enable_cache: bool,
    ) -> Self {
        Self {
            diff_tree_depth_limit,
            account_cache_size,
            storage_cache_size,
            code_cache_size,
            enable_cache,
        }
    }
}

impl Default for StateTreeConfig {
    fn default() -> Self {
        Self {
            diff_tree_depth_limit: 64,
            account_cache_size: 1000000,
            storage_cache_size: 5000000,
            code_cache_size: 100000,
            enable_cache: true,
        }
    }
}

#[derive(Clone, Debug)]
struct StateIndex {
    /// Pre-built read view for the current head. The linked head inside the
    /// view remains available for extending the topology with a child block.
    latest: Arc<FlattenedLayerView>,
    /// blockhash -> immutable read view for every in-memory block head.
    /// Each view owns O(depth) Arc pointers, so retaining a canonical window
    /// of `depth` heads costs O(depth^2) pointers; recent forks add O(depth)
    /// each until clear_diff_map prunes their height.
    hash_diff_map: HashMap<H256, Arc<FlattenedLayerView>>,
    /// blocknum -> immutable read view for every in-memory block head.
    num_diff_map: HashMap<u64, Arc<FlattenedLayerView>>,
}

impl StateIndex {
    /// Remove diff heads lower than bottom_height from the next published
    /// snapshot. Readers continue using the previous immutable snapshot until
    /// the complete update is atomically published.
    fn clear_diff_map(&mut self, bottom_height: u64) {
        if bottom_height == 0 {
            return;
        }
        self.hash_diff_map
            .retain(|_, v| v.head().unwrap_diff_layer().block_info.header.number >= bottom_height);
        self.num_diff_map.retain(|num, _| *num >= bottom_height);
    }
}

/// [`StateTree`] is a tree structure that stores the state of the EVM.
pub struct StateTree<DB> {
    /// One immutable, atomically-published snapshot keeps latest/by-hash/
    /// by-number mutually consistent and makes the RPC read side lock-free.
    index: ArcSwap<StateIndex>,
    /// Serialize the copy-on-write publisher. Reads never acquire this lock.
    update_lock: Mutex<()>,
    /// Bottom CacheDiskLayer held directly so state_at() can read
    /// `committed_height` in O(1) instead of walking the diff chain.
    cache_layer: Arc<LinkedDiffLayer>,
    /// config stores the config of the StateTree.
    config: StateTreeConfig,
    /// db is the underlying database.
    db: DB,
}

impl<DB> StateTree<DB> {
    pub fn get_config(&self) -> StateTreeConfig {
        self.config.clone()
    }
}

impl<DB> StateTree<DB>
where
    DB: StateDBProvider,
{
    pub fn new(db: DB, config: StateTreeConfig) -> Result<Self, Error<StorageError>> {
        let mut hash_diffs = HashMap::new();
        let mut num_diffs = HashMap::new();
        let latest_state = db
            .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or(Error::NoLatestBlockInDB)?;
        let info = latest_state.block_info()?;
        let cache_layer = Arc::new(LinkedDiffLayer::CacheDiskLayer(CacheDiskLayer::new(
            config.account_cache_size,
            config.storage_cache_size,
            config.code_cache_size,
            info.header.number,
            config.enable_cache,
        )));
        let bottom_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
            info.clone(),
            BlockStorageDiff::default(),
            cache_layer.clone(),
        )));
        let bottom_view = FlattenedLayerView::build(bottom_layer);
        let latest_view = FlattenedLayerView::build(cache_layer.clone());
        info!(target:"storage", "init block info: {:?}", info);
        hash_diffs.insert(info.header.hash, bottom_view.clone());
        num_diffs.insert(info.header.number, bottom_view);
        Ok(Self {
            index: ArcSwap::from_pointee(StateIndex {
                latest: latest_view,
                hash_diff_map: hash_diffs,
                num_diff_map: num_diffs,
            }),
            update_lock: Mutex::new(()),
            cache_layer,
            config,
            db,
        })
    }
}

impl<DB> EvmStorageWrite for StateTree<DB>
where
    DB: StateDBProvider,
{
    type Error = Error<StorageError>;
    /// update_block updates the state of the StateTree.
    fn update_block(
        &self,
        block_info: BlockInfo,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error> {
        let _update_guard = self.update_lock.lock().unwrap();
        let current_index = self.index.load_full();
        if current_index
            .hash_diff_map
            .contains_key(&block_info.header.hash)
        {
            info!(target:"storage", "block {:?} already exists", block_info.header.hash);
            return Ok(());
        }
        let res = current_index
            .hash_diff_map
            .get(&block_info.header.parent_hash)
            .cloned();

        let latest_statedb = self
            .db
            .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or(Error::NoLatestBlockInDB)?;

        if let Some(parent_view) = res {
            let new_diff_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
                block_info.clone(),
                block_diff,
                parent_view.head().clone(),
            )));

            // Metadata-only; None disables refill.
            let latest_view = current_index.latest.clone();
            let latest_block_info =
                HybridStateDB::from_flattened(latest_view, &latest_statedb, None).block_info()?;
            let should_publish_as_latest =
                block_info.header.number >= latest_block_info.header.number;

            if !should_publish_as_latest {
                let new_view = FlattenedLayerView::build(new_diff_layer);
                let mut next_index = (*current_index).clone();
                next_index
                    .hash_diff_map
                    .insert(block_info.header.hash, new_view.clone());
                next_index
                    .num_diff_map
                    .insert(block_info.header.number, new_view);
                self.index.store(Arc::new(next_index));
                info!(target:"storage", "import reorg block {:?} -> {:?}", block_info.header.number, latest_block_info.header.number);
                return Ok(());
            }

            BLOCK_METRICS.block_num.set(block_info.header.number as f64);
            BLOCK_METRICS
                .block_time
                .set(block_info.header.timestamp as f64);

            let bottom_height = new_diff_layer
                .clone()
                .cap_diff_to_db(self.config.diff_tree_depth_limit, latest_statedb)?;

            // cap_diff_to_db may retarget the linked chain. Build the view
            // afterwards so indexes and latest publish the final topology.
            let new_view = FlattenedLayerView::build(new_diff_layer);
            let mut next_index = (*current_index).clone();
            next_index
                .hash_diff_map
                .insert(block_info.header.hash, new_view.clone());
            next_index
                .num_diff_map
                .insert(block_info.header.number, new_view.clone());
            next_index.latest = new_view;
            debug!(target:"storage", "clear diff map bottom_height: {:?}", bottom_height);
            next_index.clear_diff_map(bottom_height);
            self.index.store(Arc::new(next_index));
            Ok(())
        } else {
            Err(Error::ParentBlockHashNotFound(
                block_info.header.hash,
                block_info.header.number,
                block_info.header.parent_hash,
            ))
        }
    }

    fn last_committed_block(&self) -> Result<Option<BlockInfo>, Self::Error> {
        let latest_statedb = self
            .db
            .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or(Error::NoLatestBlockInDB)?;
        let res = latest_statedb
            .block_info_arc()
            .map(|b| Some(b.as_ref().clone()))?;
        Ok(res)
    }
}

impl<DB> BlockIndex for StateTree<DB>
where
    DB: StateDBProvider + Sync + Send + 'static,
{
    type Error = Error<StorageError>;

    fn get_block_by_id_arc(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Arc<BlockInfo>>, Self::Error> {
        let index = self.index.load();
        let view = match block_id {
            BlockId::Hash(hash) => {
                if let Some(view) = index.hash_diff_map.get(&hash.block_hash).cloned() {
                    Ok(Some(view))
                } else {
                    Ok(None)
                }
            }
            BlockId::Number(number) => match number {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                    Ok(Some(index.latest.clone()))
                }
                BlockNumberOrTag::Number(num) => {
                    if let Some(view) = index.num_diff_map.get(&num).cloned() {
                        Ok(Some(view))
                    } else {
                        Ok(None)
                    }
                }
                _ => Err(Error::UnsupportedBlockId(BlockId::Number(number))),
            },
        };
        match view {
            Ok(None) => {
                let statedb = self.db.state_at(block_id)?;
                match statedb {
                    Some(statedb) => Ok(Some(statedb.block_info_arc()?)),
                    None => Ok(None),
                }
            }
            Ok(Some(view)) => {
                if let LinkedDiffLayer::DiffLayer(layer) = view.head().as_ref() {
                    return Ok(Some(layer.block_info.clone()));
                }
                let statedb = self
                    .db
                    .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
                    .ok_or(Error::NoLatestBlockInDB)?;
                Ok(Some(
                    HybridStateDB::from_flattened(view, statedb, None).block_info_arc()?,
                ))
            }
            Err(e) => Err(e),
        }
    }
}

impl<DB> EvmStorageRead for StateTree<DB>
where
    DB: StateDBProvider + Send + Sync + Debug + 'static,
{
    type Error = Error<StorageError>;
    type StateDB = HybridStateDB<StateDBWrapper<<DB as StateDBProvider>::StateDBReadWrite>>;

    fn state_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDB>, Self::Error> {
        let index = self.index.load();
        let view = match block_arg {
            BlockId::Hash(hash) => {
                if let Some(view) = index.hash_diff_map.get(&hash.block_hash).cloned() {
                    Ok(Some(view))
                } else {
                    Ok(None)
                }
            }
            BlockId::Number(number) => match number {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                    Ok(Some(index.latest.clone()))
                }
                BlockNumberOrTag::Number(num) => {
                    if let Some(view) = index.num_diff_map.get(&num).cloned() {
                        Ok(Some(view))
                    } else {
                        Ok(None)
                    }
                }
                _ => Err(Error::UnsupportedBlockId(BlockId::Number(number))),
            },
        };
        match view {
            Ok(None) => {
                let statedb = self.db.state_at(block_arg)?;
                match statedb {
                    Some(statedb) => Ok(Some(HybridStateDB::from_flattened(
                        FlattenedLayerView::empty(),
                        statedb,
                        None,
                    ))),
                    None => Ok(None),
                }
            }
            Ok(Some(view)) => {
                // Snapshot committed_height for refill tagging; `None`
                // when the cache is disabled so HybridStateDB skips
                // refill entirely.
                let cache_view_height = self.cache_layer.unwrap_cache_layer().cache_view_height();
                let statedb = self
                    .db
                    .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
                    .ok_or(Error::NoLatestBlockInDB)?;
                Ok(Some(HybridStateDB::from_flattened(
                    view,
                    statedb,
                    cache_view_height,
                )))
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_impl::{MultiStorage, StorageKind};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn hash(byte: u8) -> H256 {
        H256::repeat_byte(byte)
    }

    fn block_info(number: u64, hash: H256, parent_hash: H256) -> BlockInfo {
        let mut info = BlockInfo::default();
        info.inner.header.hash = hash;
        info.inner.header.inner.number = number;
        info.inner.header.inner.parent_hash = parent_hash;
        info
    }

    #[test]
    fn update_block_builds_one_post_cap_view_shared_by_all_reads() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db_path = std::env::temp_dir().join(format!(
            "leafage-flattened-view-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&db_path);

        let db =
            MultiStorage::open(&db_path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
        let genesis = block_info(0, hash(0xa0), H256::ZERO);
        StateDBWrapper(
            db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                .unwrap()
                .unwrap(),
        )
        .update_block(genesis, BlockStorageDiff::default())
        .unwrap();

        let tree = StateTree::new(db, StateTreeConfig::new(2, 100, 100, 100, true)).unwrap();
        tree.update_block(
            block_info(1, hash(0xa1), hash(0xa0)),
            BlockStorageDiff::default(),
        )
        .unwrap();
        tree.update_block(
            block_info(2, hash(0xa2), hash(0xa1)),
            BlockStorageDiff::default(),
        )
        .unwrap();
        tree.update_block(
            block_info(3, hash(0xa3), hash(0xa2)),
            BlockStorageDiff::default(),
        )
        .unwrap();

        let index = tree.index.load();
        let latest = index.latest.clone();
        let by_hash = index.hash_diff_map.get(&hash(0xa3)).unwrap().clone();
        let by_number = index.num_diff_map.get(&3).unwrap().clone();
        assert!(Arc::ptr_eq(&latest, &by_hash));
        assert!(Arc::ptr_eq(&latest, &by_number));
        assert_eq!(latest.diff_layer_count(), 2);

        let latest_handle = tree
            .state_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap();
        let numbered_handle = tree
            .state_at(BlockId::Number(BlockNumberOrTag::Number(3)))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&latest_handle.flattened, &latest));
        assert!(Arc::ptr_eq(&numbered_handle.flattened, &latest));

        // A lower-height fork is indexed with its own prebuilt view without
        // replacing the published latest view.
        tree.update_block(
            block_info(2, hash(0xb2), hash(0xa1)),
            BlockStorageDiff::default(),
        )
        .unwrap();
        let index = tree.index.load();
        let fork = index.hash_diff_map.get(&hash(0xb2)).unwrap().clone();
        assert!(!Arc::ptr_eq(&fork, &latest));
        assert_eq!(fork.diff_layer_count(), 2);
        assert!(Arc::ptr_eq(&index.latest, &latest));

        drop(numbered_handle);
        drop(latest_handle);
        drop(tree);
        let _ = std::fs::remove_dir_all(&db_path);
    }
}
