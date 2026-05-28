use crate::db::{StateDBProvider, StateDBWrapper};
use crate::db_impl::StorageError;
use crate::interface::{BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrite};
use crate::metrics::BLOCK_METRICS;
use crate::state_tree::error::Error;
use crate::state_tree::layer::{CacheDiskLayer, DiffLayer, HybridStateDB, LinkedDiffLayer};
use leafage_evm_types::{BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff, H256};
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, RwLock};
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

/// [`StateTree`] is a tree structure that stores the state of the EVM.
pub struct StateTree<DB> {
    /// latest is a single linked list that stores the latest state of the EVM.
    /// two cases:
    /// 1. bottom disk db (when init).
    /// 2. top diff -> (top-1) diif -> ... -> bottom disk db.
    latest: RwLock<Arc<LinkedDiffLayer>>,
    /// Bottom CacheDiskLayer held directly so state_at() can read
    /// `committed_height` in O(1) instead of walking the diff chain.
    cache_layer: Arc<LinkedDiffLayer>,
    /// blockhash -> node, hash_diff_map stores all the diff layer of the EVM.
    hash_diff_map: RwLock<HashMap<H256, Arc<LinkedDiffLayer>>>,
    /// blocknum-> node, num_diff_map stores all the diff layer of the EVM.
    num_diff_map: RwLock<HashMap<u64, Arc<LinkedDiffLayer>>>,
    /// config stores the config of the StateTree.
    config: StateTreeConfig,
    /// db is the underlying database.
    db: DB,
}

impl<DB> StateTree<DB> {
    /// clear_diff_map removes the diff layer that is lower than bottom_height.
    fn clear_diff_map(&self, bottom_height: u64) {
        if bottom_height == 0 {
            return;
        }
        self.hash_diff_map
            .write()
            .unwrap()
            .retain(|_, v| v.unwrap_diff_layer().block_info.header.number >= bottom_height);
        self.num_diff_map
            .write()
            .unwrap()
            .retain(|num, _| *num >= bottom_height);
    }

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
        info!(target:"storage", "init block info: {:?}", info);
        hash_diffs.insert(info.header.hash, bottom_layer.clone());
        num_diffs.insert(info.header.number, bottom_layer.clone());
        Ok(Self {
            latest: RwLock::new(cache_layer.clone()),
            cache_layer,
            hash_diff_map: RwLock::new(hash_diffs),
            num_diff_map: RwLock::new(num_diffs),
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
        if self
            .hash_diff_map
            .read()
            .unwrap()
            .contains_key(&block_info.header.hash)
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

        let latest_statedb = self
            .db
            .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or(Error::NoLatestBlockInDB)?;

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

            // Metadata-only; None disables refill.
            let latest_block_info =
                HybridStateDB::new(self.latest.read().unwrap().clone(), &latest_statedb, None)
                    .block_info()?;
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
            let bottom_height =
                new_diff_layer.cap_diff_to_db(self.config.diff_tree_depth_limit, latest_statedb)?;
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
        let memory_layer = match block_id {
            BlockId::Hash(hash) => {
                if let Some(memory_layer) = self
                    .hash_diff_map
                    .read()
                    .unwrap()
                    .get(&hash.block_hash)
                    .cloned()
                {
                    Ok(Some(memory_layer))
                } else {
                    Ok(None)
                }
            }
            BlockId::Number(number) => match number {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                    Ok(Some(self.latest.read().unwrap().clone()))
                }
                BlockNumberOrTag::Number(num) => {
                    if let Some(memory_layer) = self.num_diff_map.read().unwrap().get(&num).cloned()
                    {
                        Ok(Some(memory_layer))
                    } else {
                        Ok(None)
                    }
                }
                _ => Err(Error::UnsupportedBlockId(BlockId::Number(number))),
            },
        };
        match memory_layer {
            Ok(None) => {
                let statedb = self.db.state_at(block_id)?;
                match statedb {
                    Some(statedb) => Ok(Some(
                        HybridStateDB::new(Arc::new(LinkedDiffLayer::Empty), statedb, None)
                            .block_info_arc()?,
                    )),
                    None => Ok(None),
                }
            }
            Ok(Some(memory_layer)) => {
                match memory_layer.as_ref() {
                    LinkedDiffLayer::DiffLayer(layer) => {
                        return Ok(Some(layer.block_info.clone()));
                    }
                    _ => {}
                }
                let statedb = self
                    .db
                    .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
                    .ok_or(Error::NoLatestBlockInDB)?;
                Ok(Some(
                    HybridStateDB::new(memory_layer, statedb, None).block_info_arc()?,
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
        let memory_layer = match block_arg {
            BlockId::Hash(hash) => {
                if let Some(memory_layer) = self
                    .hash_diff_map
                    .read()
                    .unwrap()
                    .get(&hash.block_hash)
                    .cloned()
                {
                    Ok(Some(memory_layer))
                } else {
                    Ok(None)
                }
            }
            BlockId::Number(number) => match number {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                    Ok(Some(self.latest.read().unwrap().clone()))
                }
                BlockNumberOrTag::Number(num) => {
                    if let Some(memory_layer) = self.num_diff_map.read().unwrap().get(&num).cloned()
                    {
                        Ok(Some(memory_layer))
                    } else {
                        Ok(None)
                    }
                }
                _ => Err(Error::UnsupportedBlockId(BlockId::Number(number))),
            },
        };
        match memory_layer {
            Ok(None) => {
                let statedb = self.db.state_at(block_arg)?;
                match statedb {
                    Some(statedb) => Ok(Some(HybridStateDB::new(
                        Arc::new(LinkedDiffLayer::Empty),
                        statedb,
                        None,
                    ))),
                    None => Ok(None),
                }
            }
            Ok(Some(memory_layer)) => {
                // Snapshot committed_height for refill tagging; `None`
                // when the cache is disabled so HybridStateDB skips
                // refill entirely.
                let cache_view_height = self.cache_layer.unwrap_cache_layer().cache_view_height();
                let statedb = self
                    .db
                    .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
                    .ok_or(Error::NoLatestBlockInDB)?;
                Ok(Some(HybridStateDB::new(
                    memory_layer,
                    statedb,
                    cache_view_height,
                )))
            }
            Err(e) => Err(e),
        }
    }
}
