use crate::interface::{EvmStorageRead, EvmStorageWrite, StateDB};
use crate::linked_diff::error::Error;
use crate::linked_diff::layer::{CacheLayer, DiffLayer, LinkedDiffLayer};
use arc_swap::ArcSwap;
use leafage_evm_types::{BlockDiff, BlockInfo};
use reth_primitives::{BlockId, H256};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tracing::debug;

const DEFAULT_DIFF_TREE_DEPTH_LIMIT: usize = 128;

// 1GB
const DEFAULT_MEMORY_LIMIT: usize = 1 << 30;

pub struct LinkedDiffTree<DB> {
    latest_diff_layer: ArcSwap<LinkedDiffLayer<DB>>,
    cache_layer: ArcSwap<LinkedDiffLayer<DB>>,
    diffs: RwLock<HashMap<H256, Arc<LinkedDiffLayer<DB>>>>,
}

impl<DB> LinkedDiffTree<DB>
where
    DB: StateDB,
{
    pub fn new(db: DB) -> Result<Self, DB::Error> {
        let mut diffs = HashMap::new();
        let info = db.latest_block_info()?;
        let disk_layer = Arc::new(LinkedDiffLayer::DiskLayer(db));
        diffs.insert(info.hash, disk_layer.clone());
        Ok(Self {
            latest_diff_layer: ArcSwap::new(disk_layer.clone()),
            cache_layer: ArcSwap::new(disk_layer.clone()),
            diffs: RwLock::new(diffs),
        })
    }
}

impl<DB> EvmStorageWrite for LinkedDiffTree<DB>
where
    DB: EvmStorageWrite,
{
    type Error = Error<DB::Error>;
    fn update_block(
        &self,
        block_info: BlockInfo,
        block_diff: BlockDiff,
    ) -> Result<(), Self::Error> {
        if let Some(_) = self.diffs.read().unwrap().get(&block_info.hash) {
            debug!("block {} already exists", block_info.hash);
            return Ok(());
        }
        if let Some(parent_layer) = self.diffs.read().unwrap().get(&block_info.parent_root) {
            let new_diff_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
                block_info.clone(),
                block_diff.clone(),
                parent_layer.clone(),
            )));
            self.diffs
                .write()
                .unwrap()
                .insert(block_info.hash, new_diff_layer.clone());

            let cache_layer = self.cache_layer.load().clone();
            self.latest_diff_layer.store(new_diff_layer.clone());
            let mut hashes =
                new_diff_layer.write_disk_and_clear_layers(DEFAULT_DIFF_TREE_DEPTH_LIMIT)?;

            if cache_layer.is_disk_layer() {
                let bottom_cache_layer =
                    Arc::new(LinkedDiffLayer::CacheLayer(CacheLayer::new(cache_layer)));
                let top_cache_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
                    block_info.clone(),
                    block_diff,
                    bottom_cache_layer,
                )));
                self.cache_layer.store(top_cache_layer);
            } else {
                let top_cache_layer = cache_layer.unwrap_diff_layer();
                let bottom_cache_layer = top_cache_layer.next.load().clone();
                bottom_cache_layer
                    .unwrap_cache_layer()
                    .update(top_cache_layer);
                let new_top_cache_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
                    block_info.clone(),
                    block_diff,
                    bottom_cache_layer.clone(),
                )));
                let pop_hashes = bottom_cache_layer
                    .unwrap_cache_layer()
                    .pop(DEFAULT_DIFF_TREE_DEPTH_LIMIT, DEFAULT_MEMORY_LIMIT);
                hashes.extend(pop_hashes);
                self.cache_layer.store(new_top_cache_layer);
            }

            self.diffs
                .write()
                .unwrap()
                .retain(|k, _| hashes.contains(k));
            Ok(())
        } else {
            Err(Error::ParentBlockHashNotFound)
        }
    }
}

impl<DB> EvmStorageRead for LinkedDiffTree<DB>
where
    DB: StateDB,
{
    type Error = Error<DB::Error>;
    type StateDB = Arc<LinkedDiffLayer<DB>>;
    fn state_at(
        &self,
        block_arg: BlockId,
    ) -> Result<Option<(BlockInfo, Self::StateDB)>, Self::Error> {
        match block_arg {
            BlockId::Hash(hash) => {
                if let Some(layer) = self.diffs.read().unwrap().get(&hash.block_hash) {
                    if layer.is_diff_layer() {
                        return Ok(Some((
                            layer.unwrap_diff_layer().block_info.clone(),
                            layer.clone(),
                        )));
                    }
                }
                return Ok(None);
            }
            BlockId::Number(number) => {
                let cache_diff_layer = self.cache_layer.load().clone();
                if number.is_latest() {
                    return Ok(Some((
                        cache_diff_layer.unwrap_diff_layer().block_info.clone(),
                        cache_diff_layer,
                    )));
                }
                // if let Some(number) = number.as_number() {
                //     let hash = flatten_diff_layer.block_hash(U256::from(number))?;
                //     if let Some(layer) = self.diffs.read().unwrap().get(&hash) {
                //         if layer.is_diff_layer() {
                //             return Ok(Some((
                //                 layer.unwrap_diff_layer().block_info.clone(),
                //                 layer.clone(),
                //             )));
                //         }
                //     }
                // }
                return Ok(None);
            }
        };
    }
}
