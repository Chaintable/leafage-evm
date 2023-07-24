use arc_swap::ArcSwap;
use revm::db::{AccountState, DatabaseRef, DbAccount};
use revm::primitives::{AccountInfo, Bytecode, B160, B256, U256};
use std::collections::HashMap;

enum LinkedDiffLayer<DB: DatabaseRef> {
    DiskLayer(DB),
    DiffLayer(DiffLayer<DB>),
}

struct DiffLayer<DB: DatabaseRef> {
    pub accounts: HashMap<B160, DbAccount>,
    pub contracts: HashMap<B256, Bytecode>,
    pub block_hashes: HashMap<U256, B256>,
    pub next: ArcSwap<LinkedDiffLayer<DB>>,
}

impl<DB: DatabaseRef> DatabaseRef for DiffLayer<DB> {
    type Error = DB::Error;

    fn basic(&self, address: B160) -> Result<Option<AccountInfo>, Self::Error> {
        match self.accounts.get(&address) {
            Some(account) => Ok(account.info()),
            None => {
                let next = self.next.load();
                next.basic(address)
            }
        }
    }

    fn storage(&self, address: B160, index: U256) -> Result<U256, Self::Error> {
        match self.accounts.get(&address) {
            Some(acc_entry) => match acc_entry.storage.get(&index) {
                Some(entry) => Ok(*entry),
                None => {
                    if matches!(
                        acc_entry.account_state,
                        AccountState::StorageCleared | AccountState::NotExisting
                    ) {
                        Ok(U256::ZERO)
                    } else {
                        self.next.load().storage(address, index)
                    }
                }
            },
            None => self.next.load().storage(address, index),
        }
    }

    fn code_by_hash(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        match self.contracts.get(&code_hash) {
            Some(entry) => Ok(entry.clone()),
            None => self.next.load().code_by_hash(code_hash),
        }
    }

    fn block_hash(&self, number: U256) -> Result<B256, Self::Error> {
        match self.block_hashes.get(&number) {
            Some(entry) => Ok(*entry),
            None => self.next.load().block_hash(number),
        }
    }
}

impl<DB: DatabaseRef> DatabaseRef for LinkedDiffLayer<DB> {
    type Error = DB::Error;

    fn basic(&self, address: B160) -> Result<Option<AccountInfo>, Self::Error> {
        match self {
            LinkedDiffLayer::DiskLayer(db) => db.basic(address),
            LinkedDiffLayer::DiffLayer(diff) => diff.basic(address),
        }
    }

    fn storage(&self, address: B160, index: U256) -> Result<U256, Self::Error> {
        match self {
            LinkedDiffLayer::DiskLayer(db) => db.storage(address, index),
            LinkedDiffLayer::DiffLayer(diff) => diff.storage(address, index),
        }
    }

    fn code_by_hash(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        match self {
            LinkedDiffLayer::DiskLayer(db) => db.code_by_hash(code_hash),
            LinkedDiffLayer::DiffLayer(diff) => diff.code_by_hash(code_hash),
        }
    }

    fn block_hash(&self, number: U256) -> Result<B256, Self::Error> {
        match self {
            LinkedDiffLayer::DiskLayer(db) => db.block_hash(number),
            LinkedDiffLayer::DiffLayer(diff) => diff.block_hash(number),
        }
    }
}
