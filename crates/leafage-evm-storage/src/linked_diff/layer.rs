use crate::interface::{EvmStorageWrite, StateDB};
use crate::linked_diff::error::Error;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use leafage_evm_types::{
    AccountDiff, BlockDiff, BlockInfo, IndexValuePair, RawAccount, RawAccountChange,
};
use reth_primitives::H256;
use revm::primitives::{AccountInfo, Bytecode, B160, B256, U256};
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::sync::Mutex;

pub enum LinkedDiffLayer<DB> {
    DiskLayer(DB),
    CacheLayer(CacheLayer<DB>),
    DiffLayer(DiffLayer<DB>),
}

impl<DB> LinkedDiffLayer<DB>
where
    DB: EvmStorageWrite,
{
    pub fn write_disk_and_clear_layers(
        self: Arc<Self>,
        depth_limit: usize,
    ) -> Result<HashSet<H256>, DB::Error> {
        let mut used_hashes = HashSet::new();
        let mut depth = 0;
        let mut cur = self;
        if cur.is_disk_layer() {
            return Ok(used_hashes);
        }
        used_hashes.insert(cur.unwrap_diff_layer().block_info.hash);
        depth += 1;
        let mut next = cur.unwrap_diff_layer().next.load().clone();
        if next.is_disk_layer() {
            return Ok(used_hashes);
        }
        used_hashes.insert(next.unwrap_diff_layer().block_info.hash);
        depth += 1;
        let mut next_next = next.unwrap_diff_layer().next.load().clone();
        loop {
            if next_next.is_disk_layer() {
                break;
            }
            cur = next;
            next = next_next;
            depth += 1;
            next_next = next.unwrap_diff_layer().next.load().clone();
            used_hashes.insert(next_next.unwrap_diff_layer().block_info.hash);
        }
        if depth < depth_limit {
            return Ok(used_hashes);
        }
        let (next_block_info, next_block_diff) = next.as_ref().unwrap_diff_layer().get_raw();
        let db = next_next.unwrap_disk_layer();
        db.update_block(next_block_info, next_block_diff)?;
        cur.as_ref().unwrap_diff_layer().next.store(next_next);
        Ok(used_hashes)
    }
}

impl<DB> LinkedDiffLayer<DB> {
    #[inline]
    pub fn is_disk_layer(&self) -> bool {
        match self {
            LinkedDiffLayer::DiffLayer(_) => false,
            _ => true,
        }
    }

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
            LinkedDiffLayer::CacheLayer(_) => true,
            _ => false,
        }
    }

    #[inline]
    pub fn unwrap_diff_layer(&self) -> &DiffLayer<DB> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => diff,
            _ => panic!("unwrap_diff_layer"),
        }
    }

    #[inline]
    pub fn unwrap_disk_layer(&self) -> &DB {
        match self {
            LinkedDiffLayer::DiskLayer(db) => db,
            _ => panic!("unwrap_disk_layer"),
        }
    }

    #[inline]
    pub fn unwrap_cache_layer(&self) -> &CacheLayer<DB> {
        match self {
            LinkedDiffLayer::CacheLayer(cache) => cache,
            _ => panic!("unwrap_cache_layer"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DashDbAccount {
    pub info: AccountInfo,
    /// storage slots
    pub storage: DashMap<U256, U256>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ValueWithBlockHash<T> {
    value: T,
    block_hash: H256,
}

impl<T> ValueWithBlockHash<T> {
    pub fn new(value: T, block_hash: H256) -> Self {
        Self { value, block_hash }
    }
}

pub struct CacheLayer<DB> {
    pub accounts: DashMap<B160, ValueWithBlockHash<Option<AccountInfo>>>,
    pub storage: DashMap<(B160, U256), ValueWithBlockHash<U256>>,
    pub contracts: DashMap<B256, ValueWithBlockHash<Bytecode>>,
    pub block_hashes: DashMap<U256, ValueWithBlockHash<B256>>,
    pub size: AtomicUsize,
    pub diff_layer_list: Mutex<VecDeque<Arc<LinkedDiffLayer<DB>>>>,
    pub db: ArcSwap<LinkedDiffLayer<DB>>,
}

impl<DB> CacheLayer<DB> {
    pub fn new(db: Arc<LinkedDiffLayer<DB>>) -> Self {
        Self {
            accounts: DashMap::new(),
            storage: DashMap::new(),
            contracts: DashMap::new(),
            block_hashes: DashMap::new(),
            size: AtomicUsize::new(0),
            diff_layer_list: Mutex::new(VecDeque::new()),
            db: ArcSwap::new(db),
        }
    }

    pub fn pop(&self, depth_limit: usize, memory_limit: usize) -> Vec<B256> {
        let mut size_change = 0i64;
        let mut remove_hashes = Vec::new();
        if self.diff_layer_list.lock().unwrap().len() < depth_limit {
            return remove_hashes;
        }
        let mut diff_layer_list = self.diff_layer_list.lock().unwrap();
        loop {
            if self.size.load(std::sync::atomic::Ordering::SeqCst) < memory_limit {
                return remove_hashes;
            }
            let linked_diff_layer = diff_layer_list.pop_back().unwrap();
            let diff_layer = linked_diff_layer.unwrap_diff_layer();
            remove_hashes.push(diff_layer.block_info.hash);
            for (key, _) in diff_layer.accounts.iter() {
                let accout_hash = self.accounts.get(key).map(|v| v.block_hash);
                if let Some(accout_hash) = accout_hash {
                    if accout_hash == diff_layer.block_info.hash {
                        let value = self.accounts.remove(key).unwrap();
                        size_change -= std::mem::size_of_val(&key) as i64;
                        size_change -= std::mem::size_of_val(&value) as i64;
                    }
                }
            }
            for (key, _) in diff_layer.storage.iter() {
                let storage_hash = self.storage.get(key).map(|v| v.block_hash);
                if let Some(storage_hash) = storage_hash {
                    if storage_hash == diff_layer.block_info.hash {
                        let value = self.storage.remove(key).unwrap();
                        size_change -= std::mem::size_of_val(&value) as i64;
                        size_change -= std::mem::size_of_val(&key) as i64;
                    }
                }
            }
            for (key, _) in diff_layer.contracts.iter() {
                let contract_hash = self.contracts.get(key).map(|v| v.block_hash);
                if let Some(contract_hash) = contract_hash {
                    if contract_hash == diff_layer.block_info.hash {
                        let value = self.contracts.remove(key).unwrap();
                        size_change -= std::mem::size_of_val(&key) as i64;
                        size_change -= std::mem::size_of_val(&value) as i64;
                    }
                }
            }
            let block_hash = self
                .block_hashes
                .get(&diff_layer.block_info.number)
                .map(|v| v.block_hash);
            if let Some(block_hash) = block_hash {
                if block_hash == diff_layer.block_info.hash {
                    let value = self.block_hashes.remove(&diff_layer.block_info.number);
                    size_change -= std::mem::size_of_val(&diff_layer.block_info.number) as i64;
                    size_change -= std::mem::size_of_val(&value) as i64;
                }
            }
            if size_change > 0 {
                self.size
                    .fetch_add(size_change as usize, std::sync::atomic::Ordering::SeqCst);
            } else {
                self.size
                    .fetch_sub(-size_change as usize, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }

    pub fn update(&self, diff_layer: &DiffLayer<DB>) {
        let mut size_change = 0i64;
        for (key, value) in diff_layer.accounts.iter() {
            let old_value = self.accounts.remove(&key);
            if let Some(old_value) = old_value {
                size_change -= std::mem::size_of_val(&key) as i64;
                size_change -= std::mem::size_of_val(&old_value) as i64;
            }
            let value = ValueWithBlockHash::new(value.clone(), diff_layer.block_info.hash);
            size_change += std::mem::size_of_val(&key) as i64;
            size_change += std::mem::size_of_val(&value) as i64;
            self.accounts.insert(key.clone(), value);
        }
        for (key, value) in diff_layer.storage.iter() {
            let old_value = self.storage.remove(&key);
            if let Some(old_value) = old_value {
                size_change -= std::mem::size_of_val(&key) as i64;
                size_change -= std::mem::size_of_val(&old_value) as i64;
            }
            let value = ValueWithBlockHash::new(value.clone(), diff_layer.block_info.hash);
            size_change += std::mem::size_of_val(&key) as i64;
            size_change += std::mem::size_of_val(&value) as i64;
            self.storage.insert(key.clone(), value);
        }
        for (key, value) in diff_layer.contracts.iter() {
            let old_value = self.contracts.remove(&key);
            if let Some(old_value) = old_value {
                size_change -= std::mem::size_of_val(&key) as i64;
                size_change -= std::mem::size_of_val(&old_value) as i64;
            }
            let value = ValueWithBlockHash::new(value.clone(), diff_layer.block_info.hash);
            size_change += std::mem::size_of_val(&key) as i64;
            size_change += std::mem::size_of_val(&value) as i64;
            self.contracts.insert(key.clone(), value);
        }
        if size_change > 0 {
            self.size
                .fetch_add(size_change as usize, std::sync::atomic::Ordering::SeqCst);
        } else {
            self.size
                .fetch_sub(-size_change as usize, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

pub struct DiffLayer<DB> {
    pub block_info: BlockInfo,
    pub accounts: HashMap<B160, Option<AccountInfo>>,
    pub storage: HashMap<(B160, U256), U256>,
    pub contracts: HashMap<B256, Bytecode>,
    pub next: ArcSwap<LinkedDiffLayer<DB>>,
}

impl<DB> From<(BlockInfo, BlockDiff, Arc<LinkedDiffLayer<DB>>)> for DiffLayer<DB> {
    fn from(
        (block_info, block_diff, db): (BlockInfo, BlockDiff, Arc<LinkedDiffLayer<DB>>),
    ) -> Self {
        Self::new(block_info, block_diff, db)
    }
}

impl<DB> Into<(BlockInfo, BlockDiff)> for &DiffLayer<DB> {
    fn into(self) -> (BlockInfo, BlockDiff) {
        self.get_raw()
    }
}

impl<DB> DiffLayer<DB> {
    pub fn new(
        block_info: BlockInfo,
        block_diff: BlockDiff,
        next: Arc<LinkedDiffLayer<DB>>,
    ) -> Self {
        let mut accounts = HashMap::new();
        let mut storage = HashMap::new();
        let mut contracts = HashMap::new();
        for account in block_diff.accounts_diff {
            let address = account.address;
            let account_info: Option<AccountInfo> = account.info.map(|info| info.into());
            if let Some(account_info) = account_info.as_ref() {
                if let Some(code) = account_info.code.as_ref() {
                    contracts.insert(account_info.code_hash, code.clone());
                }
            }
            accounts.insert(address, account_info);
        }
        for account_diff in block_diff.storage_diff {
            let address = account_diff.account_addr;
            for index_value in account_diff.value {
                storage.insert((address, index_value.index), index_value.value);
            }
        }
        Self {
            block_info,
            accounts,
            storage,
            contracts,
            next: ArcSwap::new(next),
        }
    }

    fn get_raw(&self) -> (BlockInfo, BlockDiff) {
        let mut accounts_diff = Vec::new();
        let mut storage_diff = Vec::new();
        for (address, account) in self.accounts.iter() {
            let info = account.as_ref().map(|info| RawAccount::from(info.clone()));
            accounts_diff.push(RawAccountChange {
                address: *address,
                info,
            });
        }
        for ((address, index), value) in self.storage.iter() {
            let mut account_diff = AccountDiff::default();
            account_diff.account_addr = *address;
            account_diff.value.push(IndexValuePair {
                index: *index,
                value: *value,
            });
            storage_diff.push(account_diff);
        }
        let block_info = self.block_info.clone();
        let block_diff = BlockDiff {
            root: self.block_info.root,
            parent_root: self.block_info.parent_root,
            accounts_diff,
            storage_diff,
        };
        return (block_info, block_diff);
    }
}

impl<DB: StateDB> StateDB for LinkedDiffLayer<DB> {
    type Error = Error<DB::Error>;

    fn basic(&self, address: B160) -> Result<Option<AccountInfo>, Self::Error> {
        match self {
            LinkedDiffLayer::DiskLayer(db) => {
                let res = db.basic(address)?;
                Ok(res)
            }
            LinkedDiffLayer::DiffLayer(diff) => match diff.accounts.get(&address) {
                Some(account) => Ok(account.clone()),
                None => {
                    let next = diff.next.load();
                    next.basic(address)
                }
            },
            LinkedDiffLayer::CacheLayer(cache) => match cache.accounts.get(&address) {
                Some(account) => Ok(account.value.clone()),
                None => {
                    let next = cache.db.load();
                    next.basic(address)
                }
            },
        }
    }

    fn storage(&self, address: B160, index: U256) -> Result<U256, Self::Error> {
        match self {
            LinkedDiffLayer::DiskLayer(db) => {
                let res = db.storage(address, index)?;
                Ok(res)
            }
            LinkedDiffLayer::DiffLayer(diff) => match diff.storage.get(&(address, index)) {
                Some(value) => Ok(*value),
                None => {
                    let next = diff.next.load();
                    next.storage(address, index)
                }
            },
            LinkedDiffLayer::CacheLayer(cache) => match cache.storage.get(&(address, index)) {
                Some(value) => Ok(value.value),
                None => {
                    let next = cache.db.load();
                    next.storage(address, index)
                }
            },
        }
    }

    fn code_by_hash(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        match self {
            LinkedDiffLayer::DiskLayer(db) => {
                let res = db.code_by_hash(code_hash)?;
                Ok(res)
            }
            LinkedDiffLayer::DiffLayer(diff) => match diff.contracts.get(&code_hash) {
                Some(entry) => Ok(entry.clone()),
                None => diff.next.load().code_by_hash(code_hash),
            },
            LinkedDiffLayer::CacheLayer(cache) => match cache.contracts.get(&code_hash) {
                Some(entry) => Ok(entry.value.clone()),
                None => cache.db.load().code_by_hash(code_hash),
            },
        }
    }

    fn block_hash(&self, number: U256) -> Result<B256, Self::Error> {
        match self {
            LinkedDiffLayer::DiskLayer(db) => {
                let res = db.block_hash(number)?;
                Ok(res)
            }
            LinkedDiffLayer::DiffLayer(diff) => {
                if number == U256::from(diff.block_info.number) {
                    Ok(diff.block_info.hash)
                } else {
                    diff.next.load().block_hash(number)
                }
            }
            LinkedDiffLayer::CacheLayer(cache) => match cache.block_hashes.get(&number) {
                Some(entry) => Ok(entry.value),
                None => cache.db.load().block_hash(number),
            },
        }
    }
    fn latest_block_info(&self) -> Result<BlockInfo, Self::Error> {
        match self {
            LinkedDiffLayer::DiskLayer(db) => {
                let res = db.latest_block_info()?;
                Ok(res)
            }
            LinkedDiffLayer::DiffLayer(diff) => Ok(diff.block_info.clone()),
            LinkedDiffLayer::CacheLayer(cache) => {
                let layer_list = cache.diff_layer_list.lock().unwrap();
                let last = layer_list.back().unwrap();
                let diff = last.unwrap_diff_layer();
                Ok(diff.block_info.clone())
            }
        }
    }
}
