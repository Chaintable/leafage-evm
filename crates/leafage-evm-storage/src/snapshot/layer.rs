use crate::interface::{BlockContext, EvmStorageWrite, StateDB};
use crate::snapshot::error::Error;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use leafage_evm_types::{
    AccountInfo, AccountStorageDiff, Block, BlockStorageDiff, Bytecode, IndexValuePair, NewAccount,
    Transaction, H256, U256, U64,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tracing::info;

pub enum LinkedDiffLayer<DB> {
    CacheDiskLayer(CacheDiskLayer<DB>),
    DiffLayer(DiffLayer<DB>),
}

impl<DB> LinkedDiffLayer<DB>
where
    DB: EvmStorageWrite,
{
    pub fn cap_diff_to_db(
        self: Arc<Self>,
        depth_limit: usize,
        max_items: usize,
    ) -> Result<U64, DB::Error> {
        let cur = self;
        let mut diffs = VecDeque::new();
        diffs.push_back(cur);
        loop {
            let cur = diffs.back().unwrap();
            if cur.is_cache_layer() {
                break;
            }
            let next = cur.unwrap_diff_layer().next.load().clone();
            diffs.push_back(next);
        }
        let cache_layer = diffs.pop_back().unwrap();
        let mut bottom_num = U64::zero();
        while diffs.len() > depth_limit {
            let diff_layer = diffs.pop_back().unwrap();
            if diff_layer.unwrap_diff_layer().block_info.number.unwrap() > bottom_num {
                bottom_num = diff_layer.unwrap_diff_layer().block_info.number.unwrap();
            }
            cache_layer.unwrap_cache_layer().update(diff_layer)?;
            cache_layer.unwrap_cache_layer().pop(max_items);
        }
        Ok(bottom_num)
    }
}

impl<DB> LinkedDiffLayer<DB> {
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
    pub fn diff_layer(&self) -> Option<&DiffLayer<DB>> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => Some(diff),
            _ => None,
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
    pub fn cache_layer(&self) -> Option<&CacheDiskLayer<DB>> {
        match self {
            LinkedDiffLayer::CacheDiskLayer(cache) => Some(cache),
            _ => None,
        }
    }

    #[inline]
    pub fn unwrap_cache_layer(&self) -> &CacheDiskLayer<DB> {
        match self {
            LinkedDiffLayer::CacheDiskLayer(cache) => cache,
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

pub struct CacheDiskLayer<DB> {
    pub accounts: DashMap<H256, ValueWithBlockHash<Option<AccountInfo>>>,
    pub storage: DashMap<(H256, H256), ValueWithBlockHash<U256>>,
    pub contracts: DashMap<H256, ValueWithBlockHash<Bytecode>>,
    pub block_hashes: DashMap<U64, ValueWithBlockHash<H256>>,
    /// The diff layer list is sorted from the newest to the oldest
    pub diff_layer_list: Mutex<VecDeque<Arc<LinkedDiffLayer<DB>>>>,
    pub db: DB,
}

impl<DB: EvmStorageWrite> CacheDiskLayer<DB> {
    pub fn new(db: DB) -> Self {
        Self {
            accounts: DashMap::new(),
            storage: DashMap::new(),
            contracts: DashMap::new(),
            block_hashes: DashMap::new(),
            diff_layer_list: Mutex::new(VecDeque::new()),
            db,
        }
    }

    pub fn len(&self) -> usize {
        self.accounts.len() + self.storage.len() + self.contracts.len() + self.block_hashes.len()
    }

    pub fn pop(&self, max_items: usize) {
        loop {
            if self.len() <= max_items {
                break;
            }
            let linked_diff_layer = self.diff_layer_list.lock().unwrap().pop_back().unwrap();
            let diff_layer = linked_diff_layer.unwrap_diff_layer();
            info!(target: "storage",
                "cache pop diff layer, block number: {}, block hash: {}",
                diff_layer.block_info.number.unwrap(), diff_layer.block_info.hash.unwrap()
            );
            for (key, _) in diff_layer.accounts.iter() {
                let accout_hash = self.accounts.get(key).map(|v| v.block_hash);
                if let Some(accout_hash) = accout_hash {
                    if accout_hash == diff_layer.block_info.hash.unwrap() {
                        self.accounts.remove(key);
                    }
                }
            }
            for (key, _) in diff_layer.storage.iter() {
                let storage_hash = self.storage.get(key).map(|v| v.block_hash);
                if let Some(storage_hash) = storage_hash {
                    if storage_hash == diff_layer.block_info.hash.unwrap() {
                        self.storage.remove(key);
                    }
                }
            }
            for (key, _) in diff_layer.contracts.iter() {
                let contract_hash = self.contracts.get(key).map(|v| v.block_hash);
                if let Some(contract_hash) = contract_hash {
                    if contract_hash == diff_layer.block_info.hash.unwrap() {
                        self.contracts.remove(key);
                    }
                }
            }
            let block_hash = self
                .block_hashes
                .get(&diff_layer.block_info.number.unwrap())
                .map(|v| v.block_hash);
            if let Some(block_hash) = block_hash {
                if block_hash == diff_layer.block_info.hash.unwrap() {
                    self.block_hashes
                        .remove(&diff_layer.block_info.number.unwrap());
                }
            }
        }
    }

    pub fn update(&self, linked_layer: Arc<LinkedDiffLayer<DB>>) -> Result<(), DB::Error> {
        self.diff_layer_list
            .lock()
            .unwrap()
            .push_front(linked_layer.clone());
        let diff_layer = linked_layer.unwrap_diff_layer();
        for (key, value) in diff_layer.accounts.iter() {
            let value = ValueWithBlockHash::new(value.clone(), diff_layer.block_info.hash.unwrap());
            self.accounts.insert(key.clone(), value);
        }
        for (key, value) in diff_layer.storage.iter() {
            let value = ValueWithBlockHash::new(value.clone(), diff_layer.block_info.hash.unwrap());
            self.storage.insert(key.clone(), value);
        }
        for (key, value) in diff_layer.contracts.iter() {
            let value = ValueWithBlockHash::new(value.clone(), diff_layer.block_info.hash.unwrap());
            self.contracts.insert(key.clone(), value);
        }
        let (block_info, block_diff) = diff_layer.get_info_diff();
        self.db.update_block(block_info, block_diff)
    }
}

pub struct DiffLayer<DB> {
    pub block_info: Arc<Block<Transaction>>,
    pub accounts: HashMap<H256, Option<AccountInfo>>,
    pub storage: HashMap<(H256, H256), U256>,
    pub contracts: HashMap<H256, Bytecode>,
    pub next: ArcSwap<LinkedDiffLayer<DB>>,
}

impl<DB>
    From<(
        Block<Transaction>,
        BlockStorageDiff,
        Arc<LinkedDiffLayer<DB>>,
    )> for DiffLayer<DB>
{
    fn from(
        (block_info, block_diff, db): (
            Block<Transaction>,
            BlockStorageDiff,
            Arc<LinkedDiffLayer<DB>>,
        ),
    ) -> Self {
        Self::new(block_info, block_diff, db)
    }
}

impl<DB> Into<(Block<Transaction>, BlockStorageDiff)> for &DiffLayer<DB> {
    fn into(self) -> (Block<Transaction>, BlockStorageDiff) {
        self.get_info_diff()
    }
}

impl<DB> DiffLayer<DB> {
    pub fn new(
        block_info: Block<Transaction>,
        block_diff: BlockStorageDiff,
        next: Arc<LinkedDiffLayer<DB>>,
    ) -> Self {
        let mut accounts = HashMap::new();
        let mut storage = HashMap::new();
        let mut contracts = HashMap::new();
        for new_account in block_diff.new_accounts {
            let address = new_account.address;
            let account_info: AccountInfo = new_account.into();
            if let Some(code) = account_info.code.as_ref() {
                contracts.insert(account_info.code_hash.into(), code.clone());
            }
            accounts.insert(address, Some(account_info));
        }
        for account_diff in block_diff.storage_diff {
            let address = account_diff.account_addr;
            for index_value in account_diff.value {
                storage.insert((address, index_value.index), index_value.value);
            }
        }
        Self {
            block_info: Arc::new(block_info),
            accounts,
            storage,
            contracts,
            next: ArcSwap::new(next),
        }
    }

    fn get_info_diff(&self) -> (Block<Transaction>, BlockStorageDiff) {
        let mut new_accounts = Vec::new();
        let mut deleted_accounts = Vec::new();
        let mut storage_diff = Vec::new();
        for (address, account) in self.accounts.iter() {
            if let Some(account) = account {
                new_accounts.push(NewAccount::from((*address, account.clone())));
            } else {
                deleted_accounts.push(*address);
            }
        }
        for ((address, index), value) in self.storage.iter() {
            let mut account_diff = AccountStorageDiff {
                account_addr: *address,
                value: Vec::new(),
            };
            account_diff.value.push(IndexValuePair {
                index: *index,
                value: *value,
            });
            storage_diff.push(account_diff);
        }
        let block_info = self.block_info.clone();
        let block_diff = BlockStorageDiff {
            hash: self.block_info.hash.unwrap(),
            parent_hash: self.block_info.parent_hash,
            new_accounts,
            deleted_accounts,
            storage_diff,
        };
        (block_info.as_ref().clone(), block_diff)
    }
}

impl<DB: StateDB> StateDB for LinkedDiffLayer<DB> {
    type Error = Error<DB::Error>;

    fn basic(&self, address: H256) -> Result<Option<AccountInfo>, Self::Error> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => match diff.accounts.get(&address) {
                Some(account) => Ok(account.clone()),
                None => {
                    let next = diff.next.load();
                    next.basic(address)
                }
            },
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.accounts.get(&address) {
                Some(account) => Ok(account.value.clone()),
                None => {
                    let res = cache.db.basic(address)?;
                    Ok(res)
                }
            },
        }
    }

    fn storage(&self, address: H256, index: H256) -> Result<U256, Self::Error> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => match diff.storage.get(&(address, index)) {
                Some(value) => Ok(*value),
                None => {
                    let next = diff.next.load();
                    next.storage(address, index)
                }
            },
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.storage.get(&(address, index)) {
                Some(value) => Ok(value.value),
                None => {
                    let res = cache.db.storage(address, index)?;
                    Ok(res)
                }
            },
        }
    }

    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => match diff.contracts.get(&code_hash) {
                Some(entry) => Ok(entry.clone()),
                None => diff.next.load().code_by_hash(code_hash),
            },
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.contracts.get(&code_hash) {
                Some(entry) => Ok(entry.value.clone()),
                None => {
                    let res = cache.db.code_by_hash(code_hash)?;
                    Ok(res)
                }
            },
        }
    }

    fn block_hash(&self, number: U256) -> Result<H256, Self::Error> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => {
                if number == U256::from(diff.block_info.number.unwrap().as_u64()) {
                    Ok(diff.block_info.hash.unwrap())
                } else {
                    diff.next.load().block_hash(number)
                }
            }
            LinkedDiffLayer::CacheDiskLayer(cache) => {
                match cache.block_hashes.get(&U64::from(number.as_u64())) {
                    Some(entry) => Ok(entry.value),
                    None => {
                        let res = cache.db.block_hash(number)?;
                        Ok(res)
                    }
                }
            }
        }
    }
}

impl<DB: BlockContext> BlockContext for LinkedDiffLayer<DB> {
    type Error = Error<DB::Error>;

    fn block_info_arc(&self) -> Result<Arc<Block<Transaction>>, Self::Error> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => Ok(diff.block_info.clone()),
            LinkedDiffLayer::CacheDiskLayer(cache) => {
                let last_diff = cache.diff_layer_list.lock().unwrap().front().cloned();
                if let Some(last_diff) = last_diff {
                    return Ok(last_diff.unwrap_diff_layer().block_info.clone());
                }
                let res = cache.db.block_info_arc()?;
                Ok(res)
            }
        }
    }
}
