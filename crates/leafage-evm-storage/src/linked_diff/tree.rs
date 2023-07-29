use crate::interface::{EvmStorageRead, EvmStorageWrite};
use crate::linked_diff::error::Error;
use crate::linked_diff::layer::{DiffLayer, LinkedDiffLayer};
use arc_swap::{ArcSwap, ArcSwapOption};
use leafage_evm_types::{BlockDiff, BlockInfo};
use reth_primitives::{BlockId, H256, U256};
use revm::db::DatabaseRef;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

const DEFAULT_DIFF_TREE_DEPTH_LIMIT: usize = 128;

pub struct LinkedDiffTree<DB> {
    flatten_diff_layer: ArcSwapOption<LinkedDiffLayer<DB>>,
    latest_diff_layer: ArcSwapOption<LinkedDiffLayer<DB>>,
    disk_layer: ArcSwap<LinkedDiffLayer<DB>>,
    diffs: RwLock<HashMap<H256, Arc<LinkedDiffLayer<DB>>>>,
}

impl<DB> LinkedDiffTree<DB>
where
    DB: DatabaseRef,
{
    pub fn new(db: DB) -> Result<Self, DB::Error> {
        let mut diffs = HashMap::new();
        let hash = db.block_hash(U256::from(1))?;
        let disk_layer = Arc::new(LinkedDiffLayer::DiskLayer(db));
        diffs.insert(hash, disk_layer.clone());
        Ok(Self {
            flatten_diff_layer: ArcSwapOption::from(None),
            latest_diff_layer: ArcSwapOption::from(None),
            disk_layer: ArcSwap::new(disk_layer),
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
        let latest_diff_layer = self.latest_diff_layer.load();
        if latest_diff_layer.is_none() {
            let new_diff_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
                block_info.clone(),
                block_diff,
                self.disk_layer.load().clone(),
            )));
            self.diffs
                .write()
                .unwrap()
                .insert(block_info.hash, new_diff_layer.clone());
            self.latest_diff_layer.store(Some(new_diff_layer));
            return Ok(());
        }
        if let Some(_) = self.diffs.read().unwrap().get(&block_info.hash) {
            // has been updated
            return Ok(());
        }
        if let Some(parent_layer) = self.diffs.read().unwrap().get(&block_info.parent_root) {
            let new_diff_layer = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(
                block_info.clone(),
                block_diff,
                parent_layer.clone(),
            )));
            self.diffs
                .write()
                .unwrap()
                .insert(block_info.hash, new_diff_layer.clone());
            self.latest_diff_layer.store(Some(new_diff_layer.clone()));
            if let Some(flatten_diff_layer) = self.flatten_diff_layer.load().as_ref() {
                if flatten_diff_layer.unwrap_diff_layer().block_info.hash == block_info.parent_root
                {
                    let flatten_diff_layer = new_diff_layer
                        .clone()
                        .flatten_one(flatten_diff_layer.clone());
                    self.flatten_diff_layer.store(Some(flatten_diff_layer));
                } else {
                    if let Some(flatten_diff_layer) = new_diff_layer.clone().flatten() {
                        self.flatten_diff_layer.store(Some(flatten_diff_layer));
                    }
                }
            }
            let hashes = new_diff_layer.write_disk(DEFAULT_DIFF_TREE_DEPTH_LIMIT)?;
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
    DB: DatabaseRef,
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
                let flatten_diff_layer = self.flatten_diff_layer.load().clone();
                if flatten_diff_layer.is_none() {
                    return Ok(None);
                }
                let flatten_diff_layer = flatten_diff_layer.unwrap();
                if number.is_latest() {
                    return Ok(Some((
                        flatten_diff_layer.unwrap_diff_layer().block_info.clone(),
                        flatten_diff_layer,
                    )));
                }
                if let Some(number) = number.as_number() {
                    let hash = flatten_diff_layer.block_hash(U256::from(number))?;
                    if let Some(layer) = self.diffs.read().unwrap().get(&hash) {
                        if layer.is_diff_layer() {
                            return Ok(Some((
                                layer.unwrap_diff_layer().block_info.clone(),
                                layer.clone(),
                            )));
                        }
                    }
                }
                return Ok(None);
            }
        };
    }
}
