use crate::interface::{BlockContext, EvmStorageWrite, StateDB};
use crate::state_tree::error::Error;
use leafage_evm_types::{AccountInfo, BlockInfo, BlockStorageDiff, Bytecode, H256, U256};
use quick_cache::sync::Cache;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use tracing::info;

/// [`LinkedDiffLayer`] is a single linked list that stores the state of the EVM.
///
/// two cases:
/// 1. bottom [`CacheDiskLayer`] (when init).
/// 2. top [`DiffLayer`] -> (top-1) [`DiffLayer`] -> ... -> bottom [`CacheDiskLayer`].
#[derive(Debug)]
pub enum LinkedDiffLayer {
    CacheDiskLayer(CacheDiskLayer),
    DiffLayer(DiffLayer),
    Empty,
}

impl LinkedDiffLayer {
    /// commit the diff layer to the db and return the bottom block number.
    pub fn cap_diff_to_db<DB, E>(
        self: Arc<Self>,
        depth_limit: usize,
        statedb: DB,
    ) -> Result<u64, Error<E>>
    where
        DB: EvmStorageWrite<Error = E> + BlockContext<Error = E>,
    {
        let cur = self;
        let mut diff_layers = VecDeque::new();
        diff_layers.push_back(cur);
        loop {
            let cur = diff_layers.back().unwrap();
            if cur.is_cache_layer() {
                break;
            }
            let next = cur.unwrap_diff_layer().next.read().unwrap().clone();
            diff_layers.push_back(next);
        }
        let cache_layer = diff_layers.pop_back().unwrap();
        let mut bottom_num: u64 = 0;
        while diff_layers.len() > depth_limit {
            let diff_layer = diff_layers.pop_back().unwrap();
            // commit the diff layer to the db
            bottom_num = diff_layer.unwrap_diff_layer().block_info.header.number;
            cache_layer
                .unwrap_cache_layer()
                .commit(diff_layer, &statedb)?;
            let next_diff_layer = diff_layers.back().unwrap();
            *next_diff_layer.unwrap_diff_layer().next.write().unwrap() = cache_layer.clone();
        }
        if bottom_num == 0 {
            bottom_num = HybridStateDB {
                memory_layer: cache_layer.clone(),
                statedb,
            }
            .block_info_arc()?
            .header
            .number;
        }
        Ok(bottom_num)
    }
}

impl LinkedDiffLayer {
    #[inline]
    pub fn is_diff_layer(&self) -> bool {
        match self {
            LinkedDiffLayer::DiffLayer(_) => true,
            _ => false,
        }
    }

    #[inline]
    pub fn is_cache_layer(&self) -> bool {
        match self {
            LinkedDiffLayer::CacheDiskLayer(_) => true,
            _ => false,
        }
    }

    #[inline]
    pub fn diff_layer(&self) -> Option<&DiffLayer> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => Some(diff),
            _ => None,
        }
    }

    #[inline]
    pub fn unwrap_diff_layer(&self) -> &DiffLayer {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => diff,
            _ => panic!("unwrap_diff_layer"),
        }
    }

    #[inline]
    pub fn cache_layer(&self) -> Option<&CacheDiskLayer> {
        match self {
            LinkedDiffLayer::CacheDiskLayer(cache) => Some(cache),
            _ => None,
        }
    }

    #[inline]
    pub fn unwrap_cache_layer(&self) -> &CacheDiskLayer {
        match self {
            LinkedDiffLayer::CacheDiskLayer(cache) => cache,
            _ => panic!("unwrap_cache_layer"),
        }
    }
}

/// [`CacheDiskLayer`] is the bottom layer of the linked list.
/// It stores the on-disk db of the EVM
/// It is also a cache layer, which caches the
/// (top-diff_tree_depth_limit,top-diff_tree_depth_limit-cache_tree_depth_limit] diff layers.
#[derive(Debug)]
pub struct CacheDiskLayer {
    accounts: Cache<H256, Option<AccountInfo>>,
    storages: Cache<(H256, H256), U256>,
    contracts: Cache<H256, Bytecode>,
    block_hashes: Cache<u64, H256>,
    old_diff_layer: Mutex<Option<Arc<LinkedDiffLayer>>>,
}

impl CacheDiskLayer {
    pub fn new(
        accounts_cache_size: usize,
        storage_cache_size: usize,
        contract_cache_size: usize,
    ) -> Self {
        Self {
            accounts: Cache::new(accounts_cache_size),
            storages: Cache::new(storage_cache_size),
            contracts: Cache::new(contract_cache_size),
            block_hashes: Cache::new(1_000),
            old_diff_layer: Mutex::new(None),
        }
    }

    pub fn commit<DB, E>(&self, diff_layer: Arc<LinkedDiffLayer>, db: &DB) -> Result<(), E>
    where
        DB: EvmStorageWrite<Error = E> + BlockContext<Error = E>,
    {
        let old_head = self.old_diff_layer.lock().unwrap().clone();
        if let Some(old_head) = old_head {
            assert_eq!(
                old_head.unwrap_diff_layer().block_info.header.hash,
                diff_layer.unwrap_diff_layer().block_info.header.parent_hash
            );
        }
        *self.old_diff_layer.lock().unwrap() = Some(diff_layer.clone());
        let diff_layer = diff_layer.unwrap_diff_layer();
        for (key, _value) in diff_layer.accounts.iter() {
            self.accounts.remove(key);
        }
        for (key, _value) in diff_layer.storage.iter() {
            self.storages.remove(key);
        }
        for (key, _value) in diff_layer.contracts.iter() {
            self.contracts.remove(key);
        }
        self.block_hashes.insert(
            diff_layer.block_info.header.number,
            diff_layer.block_info.header.hash,
        );
        let (block_info, block_diff) = diff_layer.storage_diff();
        info!(target: "storage",
            "commit diff layer to db, block number: {}, block hash: {}, account cache size: {}, storage cache size: {}, contract cache size: {}",
            block_info.header.number, block_info.header.hash, self.accounts.len(), self.storages.len(), self.contracts.len()
        );
        db.update_block(block_info, block_diff)
    }
}

/// [`DiffLayer`] is the top layer of the linked list.
/// It stores the diff of the EVM.
#[derive(Debug)]
pub struct DiffLayer {
    pub block_info: Arc<BlockInfo>,
    pub block_diff: Arc<BlockStorageDiff>,
    pub accounts: HashMap<H256, Option<AccountInfo>>,
    pub storage: HashMap<(H256, H256), U256>,
    pub contracts: HashMap<H256, Bytecode>,
    pub next: RwLock<Arc<LinkedDiffLayer>>,
}

impl From<(BlockInfo, BlockStorageDiff, Arc<LinkedDiffLayer>)> for DiffLayer {
    fn from(
        (block_info, block_diff, db): (BlockInfo, BlockStorageDiff, Arc<LinkedDiffLayer>),
    ) -> Self {
        Self::new(block_info, block_diff, db)
    }
}

impl Into<(BlockInfo, BlockStorageDiff)> for &DiffLayer {
    fn into(self) -> (BlockInfo, BlockStorageDiff) {
        self.storage_diff()
    }
}

impl DiffLayer {
    pub fn new(
        block_info: BlockInfo,
        block_diff: BlockStorageDiff,
        next: Arc<LinkedDiffLayer>,
    ) -> Self {
        let mut accounts = HashMap::new();
        let mut storage = HashMap::new();
        let mut contracts = HashMap::new();
        for del_account in block_diff.deleted_accounts.iter() {
            accounts.insert(del_account.clone(), None);
        }
        for new_account in block_diff.new_accounts.iter() {
            let address = new_account.address;
            let account_info: AccountInfo = new_account.clone().into();
            accounts.insert(address, Some(account_info));
        }
        for account_diff in block_diff.storage_diffs.iter() {
            let address = account_diff.address;
            for index_value in account_diff.diffs.iter() {
                storage.insert((address, index_value.index), index_value.value);
            }
        }
        for new_code in block_diff.new_codes.iter() {
            let code_hash = new_code.code_hash;
            let code = Bytecode::new_raw(new_code.code.0.clone().into());
            contracts.insert(code_hash, code);
        }
        Self {
            block_info: Arc::new(block_info),
            block_diff: Arc::new(block_diff),
            accounts,
            storage,
            contracts,
            next: RwLock::new(next),
        }
    }

    fn storage_diff(&self) -> (BlockInfo, BlockStorageDiff) {
        (
            self.block_info.as_ref().clone(),
            self.block_diff.as_ref().clone(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct HybridStateDB<DB> {
    pub memory_layer: Arc<LinkedDiffLayer>,
    pub statedb: DB,
}

impl<DB: StateDB> StateDB for HybridStateDB<DB> {
    type Error = Error<DB::Error>;

    fn basic(&self, address: H256) -> Result<Option<AccountInfo>, Self::Error> {
        Self::basic_from_layer(&self.memory_layer, address, &self.statedb)
    }

    fn storage(&self, address: H256, index: H256) -> Result<U256, Self::Error> {
        Self::storage_from_layer(&self.memory_layer, address, index, &self.statedb)
    }

    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        Self::code_from_layer(&self.memory_layer, code_hash, &self.statedb)
    }

    fn block_hash(&self, number: u64) -> Result<H256, Self::Error> {
        Self::block_hash_from_layer(&self.memory_layer, number, &self.statedb)
    }
}

impl<DB: StateDB> HybridStateDB<DB> {
    /// Helper function to recursively search for account in layers
    #[inline]
    fn basic_from_layer(
        layer: &Arc<LinkedDiffLayer>,
        address: H256,
        statedb: &DB,
    ) -> Result<Option<AccountInfo>, Error<DB::Error>> {
        match layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => match diff.accounts.get(&address) {
                Some(value) => Ok(value.clone()),
                None => {
                    let next = diff
                        .next
                        .read()
                        .expect("Failed to acquire read lock on diff layer");
                    Self::basic_from_layer(&next, address, statedb)
                }
            },
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.accounts.get(&address) {
                Some(value) => Ok(value.clone()),
                None => {
                    let res = statedb.basic(address)?;
                    cache.accounts.insert(address, res.clone());
                    Ok(res)
                }
            },
            LinkedDiffLayer::Empty => Ok(statedb.basic(address)?),
        }
    }

    /// Helper function to recursively search for storage in layers
    #[inline]
    fn storage_from_layer(
        layer: &Arc<LinkedDiffLayer>,
        address: H256,
        index: H256,
        statedb: &DB,
    ) -> Result<U256, Error<DB::Error>> {
        match layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => match diff.storage.get(&(address, index)) {
                Some(value) => Ok(*value),
                None => {
                    let next = diff
                        .next
                        .read()
                        .expect("Failed to acquire read lock on diff layer");
                    Self::storage_from_layer(&next, address, index, statedb)
                }
            },
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.storages.get(&(address, index)) {
                Some(value) => Ok(value),
                None => {
                    let res = statedb.storage(address, index)?;
                    cache.storages.insert((address, index), res);
                    Ok(res)
                }
            },
            LinkedDiffLayer::Empty => Ok(statedb.storage(address, index)?),
        }
    }

    /// Helper function to recursively search for code in layers
    #[inline]
    fn code_from_layer(
        layer: &Arc<LinkedDiffLayer>,
        code_hash: H256,
        statedb: &DB,
    ) -> Result<Bytecode, Error<DB::Error>> {
        match layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => match diff.contracts.get(&code_hash) {
                Some(value) => Ok(value.clone()),
                None => {
                    let next = diff
                        .next
                        .read()
                        .expect("Failed to acquire read lock on diff layer");
                    Self::code_from_layer(&next, code_hash, statedb)
                }
            },
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.contracts.get(&code_hash) {
                Some(value) => Ok(value.clone()),
                None => {
                    let res = statedb.code_by_hash(code_hash)?;
                    cache.contracts.insert(code_hash, res.clone());
                    Ok(res)
                }
            },
            LinkedDiffLayer::Empty => Ok(statedb.code_by_hash(code_hash)?),
        }
    }

    /// Helper function to recursively search for block hash in layers
    #[inline]
    fn block_hash_from_layer(
        layer: &Arc<LinkedDiffLayer>,
        number: u64,
        statedb: &DB,
    ) -> Result<H256, Error<DB::Error>> {
        match layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => {
                if diff.block_info.header.number == number {
                    Ok(diff.block_info.header.hash)
                } else {
                    let next = diff
                        .next
                        .read()
                        .expect("Failed to acquire read lock on diff layer");
                    Self::block_hash_from_layer(&next, number, statedb)
                }
            }
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.block_hashes.get(&number) {
                Some(value) => Ok(value),
                None => {
                    let res = statedb.block_hash(number)?;
                    cache.block_hashes.insert(number, res);
                    Ok(res)
                }
            },
            LinkedDiffLayer::Empty => Ok(statedb.block_hash(number)?),
        }
    }
}

impl<StateDB: BlockContext> BlockContext for HybridStateDB<StateDB> {
    type Error = Error<StateDB::Error>;

    fn block_info_arc(&self) -> Result<Arc<BlockInfo>, Self::Error> {
        match self.memory_layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => Ok(diff.block_info.clone()),
            LinkedDiffLayer::CacheDiskLayer(cache) => {
                let last_diff = cache.old_diff_layer.lock().unwrap().clone();
                if let Some(last_diff) = last_diff {
                    return Ok(last_diff.unwrap_diff_layer().block_info.clone());
                }
                let res = self.statedb.block_info_arc()?;
                Ok(res)
            }
            LinkedDiffLayer::Empty => {
                let res = self.statedb.block_info_arc()?;
                Ok(res)
            }
        }
    }

    fn state_diff_arc(&self) -> Result<Arc<BlockStorageDiff>, Self::Error> {
        match self.memory_layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => Ok(diff.block_diff.clone()),
            LinkedDiffLayer::CacheDiskLayer(cache) => {
                let last_diff = cache.old_diff_layer.lock().unwrap().clone();
                if let Some(last_diff) = last_diff {
                    return Ok(last_diff.unwrap_diff_layer().block_diff.clone());
                }
                let res = self.statedb.state_diff_arc()?;
                Ok(res)
            }
            LinkedDiffLayer::Empty => {
                let res = self.statedb.state_diff_arc()?;
                Ok(res)
            }
        }
    }
}
