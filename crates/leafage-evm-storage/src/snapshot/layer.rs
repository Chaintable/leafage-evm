use crate::interface::{BlockContext, EvmStorageWrite, StateDB};
use crate::snapshot::error::Error;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use leafage_evm_types::{
    AccountInfo, AccountStorageDiff, Block, BlockStorageDiff, Bytecode, IndexValuePair, NewAccount,
    NewCode, Transaction, H256, U256, U64,
};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tracing::info;

/// [`LinkedDiffLayer`] is a single linked list that stores the state of the EVM.
///
/// two cases:
/// 1. bottom [`CacheDiskLayer`] (when init).
/// 2. top [`DiffLayer`] -> (top-1) [`DiffLayer`] -> ... -> bottom [`CacheDiskLayer`].
pub enum LinkedDiffLayer<DB> {
    CacheDiskLayer(CacheDiskLayer<DB>),
    DiffLayer(DiffLayer<DB>),
}

impl<DB, E> LinkedDiffLayer<DB>
where
    DB: EvmStorageWrite<Error = E> + BlockContext<Error = E>,
{
    /// commit the diff layer to the db and return the bottom block number.
    pub fn cap_diff_to_db(
        self: Arc<Self>,
        depth_limit: usize,
        cache_depth_limit: usize,
    ) -> Result<U64, Error<E>> {
        let cur = self;
        let mut diff_layers = VecDeque::new();
        diff_layers.push_back(cur);
        loop {
            let cur = diff_layers.back().unwrap();
            if cur.is_cache_layer() {
                break;
            }
            let next = cur.unwrap_diff_layer().next.load().clone();
            diff_layers.push_back(next);
        }
        let cache_layer = diff_layers.pop_back().unwrap();
        let mut bottom_num = U64::zero();
        while diff_layers.len() > depth_limit {
            let diff_layer = diff_layers.pop_back().unwrap();
            // commit the diff layer to the db
            bottom_num = diff_layer.unwrap_diff_layer().block_info.number.unwrap();
            cache_layer.unwrap_cache_layer().commit(diff_layer)?;
            let next_diff_layer = diff_layers.back().unwrap();
            next_diff_layer
                .unwrap_diff_layer()
                .next
                .store(cache_layer.clone());
        }
        // pop the oldest cache layer
        cache_layer.unwrap_cache_layer().pop(cache_depth_limit);
        if bottom_num == U64::zero() {
            bottom_num = cache_layer.block_info_arc()?.number.unwrap();
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

#[derive(Debug, Clone, Eq, PartialEq)]
struct ValueWithBlockNumber<T> {
    value: T,
    block_num: u64,
}

impl<T> ValueWithBlockNumber<T> {
    pub fn new(value: T, block_num: u64) -> Self {
        Self { value, block_num }
    }
}

/// [`CacheDiskLayer`] is the bottom layer of the linked list.
/// It stores the on-disk db of the EVM
/// It is also a cache layer, which caches the
/// (top-diff_tree_depth_limit,top-diff_tree_depth_limit-cache_tree_depth_limit] diff layers.
pub struct CacheDiskLayer<DB> {
    accounts: DashMap<H256, ValueWithBlockNumber<Option<AccountInfo>>>,
    storage: DashMap<(H256, H256), ValueWithBlockNumber<U256>>,
    contracts: DashMap<H256, ValueWithBlockNumber<Bytecode>>,
    block_hashes: DashMap<U64, H256>,
    /// The diff layer list is sorted from the newest to the oldest
    diff_layer_list: Mutex<VecDeque<Arc<LinkedDiffLayer<DB>>>>,
    db: DB,
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

    pub fn pop(&self, max_cache_layers: usize) {
        loop {
            if self.diff_layer_list.lock().unwrap().len() <= max_cache_layers {
                break;
            }
            let linked_diff_layer = self.diff_layer_list.lock().unwrap().pop_back().unwrap();
            let diff_layer = linked_diff_layer.unwrap_diff_layer();
            info!(target: "storage",
                "cache pop diff layer, block number: {}, block hash: {}",
                diff_layer.block_info.number.unwrap(), diff_layer.block_info.hash.unwrap()
            );
            for (key, _) in diff_layer.accounts.iter() {
                let accout_block_num = self.accounts.get(key).map(|v| v.block_num);
                if let Some(accout_block_num) = accout_block_num {
                    if accout_block_num == diff_layer.block_info.number.unwrap().as_u64() {
                        self.accounts.remove(key);
                    }
                }
            }
            for (key, _) in diff_layer.storage.iter() {
                let storage_block_num = self.storage.get(key).map(|v| v.block_num);
                if let Some(storage_block_num) = storage_block_num {
                    if storage_block_num == diff_layer.block_info.number.unwrap().as_u64() {
                        self.storage.remove(key);
                    }
                }
            }
            for (key, _) in diff_layer.contracts.iter() {
                let contract_block_num = self.contracts.get(key).map(|v| v.block_num);
                if let Some(contract_block_num) = contract_block_num {
                    if contract_block_num == diff_layer.block_info.number.unwrap().as_u64() {
                        self.contracts.remove(key);
                    }
                }
            }
            self.block_hashes
                .remove(&diff_layer.block_info.number.unwrap());
        }
    }

    pub fn commit(&self, diff_layer: Arc<LinkedDiffLayer<DB>>) -> Result<(), DB::Error> {
        let old_head = self.diff_layer_list.lock().unwrap().front().cloned();
        if let Some(old_head) = old_head {
            assert_eq!(
                old_head.unwrap_diff_layer().block_info.hash.unwrap(),
                diff_layer.unwrap_diff_layer().block_info.parent_hash
            );
        }
        self.diff_layer_list
            .lock()
            .unwrap()
            .push_front(diff_layer.clone());
        let diff_layer = diff_layer.unwrap_diff_layer();
        for (key, value) in diff_layer.accounts.iter() {
            let value = ValueWithBlockNumber::new(
                value.clone(),
                diff_layer.block_info.number.unwrap().as_u64(),
            );
            self.accounts.insert(key.clone(), value);
        }
        for (key, value) in diff_layer.storage.iter() {
            let value = ValueWithBlockNumber::new(
                value.clone(),
                diff_layer.block_info.number.unwrap().as_u64(),
            );
            self.storage.insert(key.clone(), value);
        }
        for (key, value) in diff_layer.contracts.iter() {
            let value = ValueWithBlockNumber::new(
                value.clone(),
                diff_layer.block_info.number.unwrap().as_u64(),
            );
            self.contracts.insert(key.clone(), value);
        }
        self.block_hashes.insert(
            diff_layer.block_info.number.unwrap(),
            diff_layer.block_info.hash.unwrap(),
        );
        let (block_info, block_diff) = diff_layer.storage_diff();
        info!(target: "storage",
            "commit diff layer to db, block number: {}, block hash: {}",
            block_info.number.unwrap(), block_info.hash.unwrap()
        );
        self.db.update_block(block_info, block_diff)
    }
}

/// [`DiffLayer`] is the top layer of the linked list.
/// It stores the diff of the EVM.
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
        self.storage_diff()
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
            accounts.insert(address, Some(account_info));
        }
        for account_diff in block_diff.storage_diffs {
            let address = account_diff.address;
            for index_value in account_diff.diffs {
                storage.insert((address, index_value.index), index_value.value);
            }
        }
        for new_code in block_diff.new_codes {
            let code_hash = new_code.code_hash;
            let code = Bytecode::new_raw(new_code.code.0);
            assert_eq!(code_hash, code.hash().into());
            contracts.insert(code_hash, code);
        }
        Self {
            block_info: Arc::new(block_info),
            accounts,
            storage,
            contracts,
            next: ArcSwap::new(next),
        }
    }

    fn storage_diff(&self) -> (Block<Transaction>, BlockStorageDiff) {
        let mut new_accounts = Vec::new();
        let mut deleted_accounts = Vec::new();
        let mut storage_diffs = Vec::new();
        let mut new_codes = Vec::new();
        for (address, account) in self.accounts.iter() {
            if let Some(account) = account {
                new_accounts.push(NewAccount::from((*address, account.clone())));
            } else {
                deleted_accounts.push(*address);
            }
        }
        for ((address, index), value) in self.storage.iter() {
            let mut account_diff = AccountStorageDiff {
                address: *address,
                diffs: Vec::new(),
            };
            account_diff.diffs.push(IndexValuePair {
                index: *index,
                value: *value,
            });
            storage_diffs.push(account_diff);
        }
        for (code_hash, code) in self.contracts.iter() {
            new_codes.push(NewCode {
                code_hash: *code_hash,
                code: code.bytecode.clone().into(),
            });
        }
        let block_info = self.block_info.clone();
        let block_diff = BlockStorageDiff {
            hash: self.block_info.hash.unwrap(),
            parent_hash: self.block_info.parent_hash,
            new_accounts,
            deleted_accounts,
            storage_diffs,
            new_codes,
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
                    Some(entry) => Ok(*entry),
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
