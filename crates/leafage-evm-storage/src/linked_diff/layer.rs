use crate::interface::EvmStorageWrite;
use crate::linked_diff::error::Error;
use arc_swap::ArcSwap;
use leafage_evm_types::{
    AccountDiff, BlockDiff, BlockInfo, IndexValuePair, RawAccount, RawAccountChange,
};
use reth_primitives::H256;
use revm::db::{AccountState, DatabaseRef, DbAccount};
use revm::primitives::{AccountInfo, Bytecode, B160, B256, U256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub enum LinkedDiffLayer<DB> {
    DiskLayer(DB),
    DiffLayer(DiffLayer<DB>),
}

impl<DB> LinkedDiffLayer<DB>
where
    DB: EvmStorageWrite,
{
    #[inline]
    pub fn write_disk(self: Arc<Self>, depth_limit: usize) -> Result<HashSet<H256>, DB::Error> {
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
            LinkedDiffLayer::DiskLayer(_) => true,
            LinkedDiffLayer::DiffLayer(_) => false,
        }
    }

    #[inline]
    pub fn is_diff_layer(&self) -> bool {
        match self {
            LinkedDiffLayer::DiskLayer(_) => false,
            LinkedDiffLayer::DiffLayer(_) => true,
        }
    }

    #[inline]
    pub fn unwrap_diff_layer(&self) -> &DiffLayer<DB> {
        match self {
            LinkedDiffLayer::DiskLayer { .. } => panic!("unwrap_diff_layer"),
            LinkedDiffLayer::DiffLayer(diff) => diff,
        }
    }

    #[inline]
    pub fn unwrap_disk_layer(&self) -> &DB {
        match self {
            LinkedDiffLayer::DiskLayer(db) => db,
            LinkedDiffLayer::DiffLayer(_) => panic!("unwrap_disk_layer"),
        }
    }

    pub fn flatten(self: Arc<Self>) -> Option<Arc<Self>> {
        if self.is_disk_layer() {
            return None;
        }
        let this = self.unwrap_diff_layer();
        if this.next.load().is_disk_layer() {
            return None;
        }
        Some(Arc::new(LinkedDiffLayer::DiffLayer(
            this.flatten_to_oldest(),
        )))
    }

    pub fn flatten_one(self: Arc<Self>, next: Arc<Self>) -> Arc<Self> {
        if next.is_disk_layer() {
            return self;
        }
        let this = self.unwrap_diff_layer();
        let next = next.unwrap_diff_layer();
        let new_diff_layer = this.flatten_one(next);
        Arc::new(LinkedDiffLayer::DiffLayer(new_diff_layer))
    }
}

pub struct DiffLayer<DB> {
    pub block_info: BlockInfo,
    pub accounts: HashMap<B160, DbAccount>,
    pub contracts: HashMap<B256, Bytecode>,
    pub block_hashes: HashMap<U256, B256>,
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
        let mut contracts = HashMap::new();
        let mut block_hashes = HashMap::new();
        block_hashes.insert(block_info.number, block_info.hash);
        for account in block_diff.accounts_diff {
            let address = account.address;
            let account_info: Option<AccountInfo> = account.info.map(|info| info.into());
            if let Some(account_info) = account_info.as_ref() {
                if let Some(code) = account_info.code.as_ref() {
                    contracts.insert(account_info.code_hash, code.clone());
                }
            }
            let db_account = DbAccount::from(account_info);
            accounts.insert(address, db_account);
        }
        Self {
            block_info,
            accounts,
            contracts,
            block_hashes,
            next: ArcSwap::new(next),
        }
    }

    fn get_raw(&self) -> (BlockInfo, BlockDiff) {
        let mut accounts_diff = Vec::new();
        let mut storage_diff = Vec::new();
        for (address, account) in self.accounts.iter() {
            let info = account.info().map(|info| RawAccount::from(info));
            accounts_diff.push(RawAccountChange {
                address: *address,
                info,
            });
            for (index, value) in account.storage.iter() {
                let mut account_diff = AccountDiff::default();
                account_diff.account_addr = *address;
                account_diff.value.push(IndexValuePair {
                    index: *index,
                    value: *value,
                });
                storage_diff.push(account_diff);
            }
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

    fn flatten_one(&self, next: &Self) -> Self {
        let mut accounts = self.accounts.clone();
        let mut contracts = self.contracts.clone();
        let mut block_hashes = self.block_hashes.clone();
        for (address, old_account) in next.accounts.iter() {
            if let Some(account) = accounts.get_mut(address) {
                if let Some(code) = account.info.code.as_ref() {
                    if !contracts.contains_key(&account.info.code_hash) {
                        contracts.insert(account.info.code_hash, code.clone());
                    }
                }
                if matches!(
                    account.account_state,
                    AccountState::StorageCleared | AccountState::NotExisting
                ) {
                    continue;
                } else {
                    for (index, value) in old_account.storage.iter() {
                        if !account.storage.contains_key(index) {
                            account.storage.insert(*index, *value);
                        }
                    }
                }
            } else {
                accounts.insert(*address, old_account.clone());
            }
        }
        block_hashes.extend(next.block_hashes.clone());
        Self {
            block_info: self.block_info.clone(),
            accounts,
            contracts,
            block_hashes,
            next: ArcSwap::new(next.next.load().clone()),
        }
    }

    fn flatten_to_oldest(&self) -> Self {
        let mut accounts = self.accounts.clone();
        let mut contracts = self.contracts.clone();
        let mut block_hashes = self.block_hashes.clone();
        let mut next = self.next.load().clone();
        loop {
            if next.is_disk_layer() {
                break;
            }
            let next_diff = next.as_ref().unwrap_diff_layer();
            for (address, old_account) in next_diff.accounts.iter() {
                if let Some(account) = accounts.get_mut(address) {
                    if let Some(code) = account.info.code.as_ref() {
                        if !contracts.contains_key(&account.info.code_hash) {
                            contracts.insert(account.info.code_hash, code.clone());
                        }
                    }
                    if matches!(
                        account.account_state,
                        AccountState::StorageCleared | AccountState::NotExisting
                    ) {
                        continue;
                    } else {
                        for (index, value) in old_account.storage.iter() {
                            if !account.storage.contains_key(index) {
                                account.storage.insert(*index, *value);
                            }
                        }
                    }
                } else {
                    accounts.insert(*address, old_account.clone());
                }
            }
            block_hashes.insert(next_diff.block_info.number, next_diff.block_info.hash);
            next = next_diff.next.load().clone();
        }
        Self {
            block_info: self.block_info.clone(),
            accounts,
            contracts,
            block_hashes,
            next: ArcSwap::new(next),
        }
    }
}

impl<DB: DatabaseRef> DatabaseRef for LinkedDiffLayer<DB> {
    type Error = Error<DB::Error>;

    fn basic(&self, address: B160) -> Result<Option<AccountInfo>, Self::Error> {
        match self {
            LinkedDiffLayer::DiskLayer(db) => {
                let res = db.basic(address)?;
                Ok(res)
            }
            LinkedDiffLayer::DiffLayer(diff) => match diff.accounts.get(&address) {
                Some(account) => Ok(account.info()),
                None => {
                    let next = diff.next.load();
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
            LinkedDiffLayer::DiffLayer(diff) => match diff.accounts.get(&address) {
                Some(acc_entry) => match acc_entry.storage.get(&index) {
                    Some(entry) => Ok(*entry),
                    None => {
                        if matches!(
                            acc_entry.account_state,
                            AccountState::StorageCleared | AccountState::NotExisting
                        ) {
                            Ok(U256::ZERO)
                        } else {
                            diff.next.load().storage(address, index)
                        }
                    }
                },
                None => diff.next.load().storage(address, index),
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
        }
    }

    fn block_hash(&self, number: U256) -> Result<B256, Self::Error> {
        match self {
            LinkedDiffLayer::DiskLayer(db) => {
                let res = db.block_hash(number)?;
                Ok(res)
            }
            LinkedDiffLayer::DiffLayer(diff) => match diff.block_hashes.get(&number) {
                Some(entry) => Ok(*entry),
                None => diff.next.load().block_hash(number),
            },
        }
    }
}
