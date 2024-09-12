use crate::interface::{BlockContext, EvmStorageWrite, StateDB, TransactionIndex, TxContext};
use crate::metrics::{
    ACCOUNT_CACHE_HIT, ACCOUNT_CACHE_MISS, CODE_CACHE_HIT, CODE_CACHE_MISS, STORAGE_CACHE_HIT,
    STORAGE_CACHE_MISS,
};
use crate::snapshot::error::Error;
use leafage_evm_types::{AccountInfo, Block, BlockStorageDiff, Bytecode, Transaction, H256, U256};
use quick_cache::sync::Cache;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
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
    pub fn cap_diff_to_db(self: Arc<Self>, depth_limit: usize) -> Result<u64, Error<E>> {
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
            cache_layer.unwrap_cache_layer().commit(diff_layer)?;
            let next_diff_layer = diff_layers.back().unwrap();
            *next_diff_layer.unwrap_diff_layer().next.write().unwrap() = cache_layer.clone();
        }
        if bottom_num == 0 {
            bottom_num = cache_layer.block_info_arc()?.header.number;
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

/// [`CacheDiskLayer`] is the bottom layer of the linked list.
/// It stores the on-disk db of the EVM
/// It is also a cache layer, which caches the
/// (top-diff_tree_depth_limit,top-diff_tree_depth_limit-cache_tree_depth_limit] diff layers.
pub struct CacheDiskLayer<DB> {
    accounts: Cache<H256, Option<AccountInfo>>,
    storages: Cache<(H256, H256), U256>,
    contracts: Cache<H256, Bytecode>,
    block_hashes: Cache<u64, H256>,
    old_diff_layer: Mutex<Option<Arc<LinkedDiffLayer<DB>>>>,
    db: DB,
}

impl<DB: EvmStorageWrite> CacheDiskLayer<DB> {
    pub fn new(
        db: DB,
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
            db,
        }
    }

    pub fn commit(&self, diff_layer: Arc<LinkedDiffLayer<DB>>) -> Result<(), DB::Error> {
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
        self.db.update_block(block_info, block_diff)
    }
}

/// [`DiffLayer`] is the top layer of the linked list.
/// It stores the diff of the EVM.
pub struct DiffLayer<DB> {
    pub block_info: Arc<Block<Transaction>>,
    pub block_diff: Arc<BlockStorageDiff>,
    pub accounts: HashMap<H256, Option<AccountInfo>>,
    pub storage: HashMap<(H256, H256), U256>,
    pub contracts: HashMap<H256, Bytecode>,
    pub next: RwLock<Arc<LinkedDiffLayer<DB>>>,
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

    fn storage_diff(&self) -> (Block<Transaction>, BlockStorageDiff) {
        (
            self.block_info.as_ref().clone(),
            self.block_diff.as_ref().clone(),
        )
    }
}

impl<DB: StateDB> StateDB for LinkedDiffLayer<DB> {
    type Error = Error<DB::Error>;

    fn basic(&self, address: H256) -> Result<Option<AccountInfo>, Self::Error> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => match diff.accounts.get(&address) {
                Some(account) => Ok(account.clone()),
                None => {
                    let next = diff.next.read().unwrap().clone();
                    next.basic(address)
                }
            },
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.accounts.get(&address) {
                Some(account) => {
                    ACCOUNT_CACHE_HIT.inc();
                    Ok(account)
                }
                None => {
                    ACCOUNT_CACHE_MISS.inc();
                    let res = cache.db.basic(address)?;
                    cache.accounts.insert(address, res.clone());
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
                    let next = diff.next.read().unwrap().clone();
                    next.storage(address, index)
                }
            },
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.storages.get(&(address, index)) {
                Some(value) => {
                    STORAGE_CACHE_HIT.inc();
                    Ok(value)
                }
                None => {
                    STORAGE_CACHE_MISS.inc();
                    let res = cache.db.storage(address, index)?;
                    cache.storages.insert((address, index), res);
                    Ok(res)
                }
            },
        }
    }

    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => match diff.contracts.get(&code_hash) {
                Some(entry) => Ok(entry.clone()),
                None => {
                    let next = diff.next.read().unwrap().clone();
                    next.code_by_hash(code_hash)
                }
            },
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.contracts.get(&code_hash) {
                Some(entry) => {
                    CODE_CACHE_HIT.inc();
                    Ok(entry)
                }
                None => {
                    CODE_CACHE_MISS.inc();
                    let res = cache.db.code_by_hash(code_hash)?;
                    cache.contracts.insert(code_hash, res.clone());
                    Ok(res)
                }
            },
        }
    }

    fn block_hash(&self, number: u64) -> Result<H256, Self::Error> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => {
                if number == diff.block_info.header.number {
                    Ok(diff.block_info.header.hash)
                } else {
                    let next = diff.next.read().unwrap().clone();
                    next.block_hash(number)
                }
            }
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.block_hashes.get(&number) {
                Some(entry) => Ok(entry),
                None => {
                    let res = cache.db.block_hash(number)?;
                    Ok(res)
                }
            },
        }
    }
}

impl<DB: BlockContext> BlockContext for LinkedDiffLayer<DB> {
    type Error = Error<DB::Error>;

    fn block_info_arc(&self) -> Result<Arc<Block<Transaction>>, Self::Error> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => Ok(diff.block_info.clone()),
            LinkedDiffLayer::CacheDiskLayer(cache) => {
                let last_diff = cache.old_diff_layer.lock().unwrap().clone();
                if let Some(last_diff) = last_diff {
                    return Ok(last_diff.unwrap_diff_layer().block_info.clone());
                }
                let res = cache.db.block_info_arc()?;
                Ok(res)
            }
        }
    }
}

impl<DB: BlockContext> TransactionIndex for LinkedDiffLayer<DB> {
    type Error = Error<DB::Error>;

    fn get_transaction_by_hash(&self, tx_hash: H256) -> Result<Option<Transaction>, Self::Error> {
        let block_info = self.block_info_arc()?;
        for tx in block_info.transactions.txns() {
            if tx.hash == tx_hash {
                return Ok(Some(tx.clone()));
            }
        }
        Ok(None)
    }

    fn get_transaction_by_context(
        &self,
        tx_context: &TxContext,
    ) -> Result<Option<Transaction>, Self::Error> {
        let block_info = self.block_info_arc()?;
        if let Some(txns) = block_info.transactions.as_transactions() {
            let tx = txns.get(tx_context.transaction_index as usize).cloned();
            if tx.is_some() {
                return Ok(tx);
            }
            for tx in txns {
                if tx.hash == tx_context.transaction_hash {
                    return Ok(Some(tx.clone()));
                }
            }
        }
        Ok(None)
    }
}
