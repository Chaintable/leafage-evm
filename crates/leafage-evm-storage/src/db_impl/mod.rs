//! Database implementation for EVM storage.

mod rocksdb_impl;

mod mdbx_impl;

mod error;

pub use error::Error as StorageError;
pub use mdbx_impl::{MDBXStateDB, MDBXStorage, MDBXWriteBatch};
pub use rocksdb_impl::{ArchiveRocksDBStorage, ArchiveStateDB, RocksDBStorage};

use crate::db::{BlockIterator, LatestStateDBIterator, StateDBProvider, StateDBRead, StateDBWrite};
use leafage_evm_types::{Block, BlockId, Bytes, NewAccount, H256, U256};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum MultiStorage {
    RocksDBState(Arc<RocksDBStorage>),
    RocksDBArchive(Arc<ArchiveRocksDBStorage>),
    MDBXState(Arc<MDBXStorage>),
}

#[derive(Debug, Clone, Copy)]
pub enum StorageKind {
    Rocksdb,
    MDBX,
}

impl FromStr for StorageKind {
    type Err = StorageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "rocksdb" => Ok(StorageKind::Rocksdb),
            "mdbx" => Ok(StorageKind::MDBX),
            _ => Err(StorageError::UnSupported(format!(
                "unsupported storage kind: {}",
                s
            ))),
        }
    }
}

impl MultiStorage {
    pub fn open<P: AsRef<Path>>(
        path: P,
        cache_size: usize,
        kind: StorageKind,
        is_archive: bool,
    ) -> Result<Self, StorageError> {
        match (kind, is_archive) {
            (StorageKind::Rocksdb, false) => {
                let db = RocksDBStorage::open(path, cache_size);
                Ok(MultiStorage::RocksDBState(Arc::new(db)))
            }
            (StorageKind::Rocksdb, true) => {
                let db = ArchiveRocksDBStorage::open(path, cache_size);
                Ok(MultiStorage::RocksDBArchive(Arc::new(db)))
            }
            (StorageKind::MDBX, false) => {
                let db = MDBXStorage::open(path);
                Ok(MultiStorage::MDBXState(Arc::new(db)))
            }
            (StorageKind::MDBX, true) => Err(StorageError::UnSupported(
                "MDBX archive storage is not supported".to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub enum MultiStateDB {
    RocksDBState(Arc<RocksDBStorage>),
    RocksDBArchive(ArchiveStateDB),
    MDBXState(MDBXStateDB),
}

impl LatestStateDBIterator for MultiStorage {
    fn account_iter(&self) -> impl Iterator<Item = Result<(H256, NewAccount), StorageError>> {
        match self {
            MultiStorage::RocksDBState(db) => {
                Box::new(db.account_iter()) as Box<dyn Iterator<Item = _>>
            }
            MultiStorage::RocksDBArchive(db) => {
                Box::new(db.account_iter()) as Box<dyn Iterator<Item = _>>
            }
            MultiStorage::MDBXState(db) => {
                Box::new(db.account_iter()) as Box<dyn Iterator<Item = _>>
            }
        }
    }

    fn code_iter(&self) -> impl Iterator<Item = Result<(H256, Bytes), StorageError>> {
        match self {
            MultiStorage::RocksDBState(db) => {
                Box::new(db.code_iter()) as Box<dyn Iterator<Item = _>>
            }
            MultiStorage::RocksDBArchive(db) => {
                Box::new(db.code_iter()) as Box<dyn Iterator<Item = _>>
            }
            MultiStorage::MDBXState(db) => Box::new(db.code_iter()) as Box<dyn Iterator<Item = _>>,
        }
    }

    fn storage_iter(&self) -> impl Iterator<Item = Result<(H256, H256, U256), StorageError>> {
        match self {
            MultiStorage::RocksDBState(db) => {
                Box::new(db.storage_iter()) as Box<dyn Iterator<Item = _>>
            }
            MultiStorage::RocksDBArchive(db) => {
                Box::new(db.storage_iter()) as Box<dyn Iterator<Item = _>>
            }
            MultiStorage::MDBXState(db) => {
                Box::new(db.storage_iter()) as Box<dyn Iterator<Item = _>>
            }
        }
    }
}

impl BlockIterator for MultiStorage {
    fn block_info_iter(&self) -> impl Iterator<Item = Result<Block<H256>, StorageError>> {
        match self {
            MultiStorage::RocksDBState(db) => {
                Box::new(db.block_info_iter()) as Box<dyn Iterator<Item = _>>
            }
            MultiStorage::RocksDBArchive(db) => {
                Box::new(db.block_info_iter()) as Box<dyn Iterator<Item = _>>
            }
            MultiStorage::MDBXState(db) => {
                Box::new(db.block_info_iter()) as Box<dyn Iterator<Item = _>>
            }
        }
    }

    fn block_hash_iter(&self) -> impl Iterator<Item = Result<(u64, H256), StorageError>> {
        match self {
            MultiStorage::RocksDBState(db) => {
                Box::new(db.block_hash_iter()) as Box<dyn Iterator<Item = _>>
            }
            MultiStorage::RocksDBArchive(db) => {
                Box::new(db.block_hash_iter()) as Box<dyn Iterator<Item = _>>
            }
            MultiStorage::MDBXState(db) => {
                Box::new(db.block_hash_iter()) as Box<dyn Iterator<Item = _>>
            }
        }
    }
}

impl StateDBProvider for MultiStorage {
    type StateDBReadWrite = MultiStateDB;

    fn db_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDBReadWrite>, StorageError> {
        match self {
            MultiStorage::RocksDBState(db) => db
                .db_at(block_arg)
                .map(|opt| opt.map(MultiStateDB::RocksDBState)),
            MultiStorage::RocksDBArchive(db) => db
                .db_at(block_arg)
                .map(|opt| opt.map(MultiStateDB::RocksDBArchive)),
            MultiStorage::MDBXState(db) => db
                .db_at(block_arg)
                .map(|opt| opt.map(MultiStateDB::MDBXState)),
        }
    }
}

impl StateDBRead for MultiStateDB {
    fn read_account(&self, address: H256) -> Result<Option<NewAccount>, StorageError> {
        match self {
            MultiStateDB::RocksDBState(db) => db.read_account(address),
            MultiStateDB::RocksDBArchive(db) => db.read_account(address),
            MultiStateDB::MDBXState(db) => db.read_account(address),
        }
    }

    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, StorageError> {
        match self {
            MultiStateDB::RocksDBState(db) => db.read_code(code_hash),
            MultiStateDB::RocksDBArchive(db) => db.read_code(code_hash),
            MultiStateDB::MDBXState(db) => db.read_code(code_hash),
        }
    }

    fn read_storage(&self, address: H256, key: H256) -> Result<U256, StorageError> {
        match self {
            MultiStateDB::RocksDBState(db) => db.read_storage(address, key),
            MultiStateDB::RocksDBArchive(db) => db.read_storage(address, key),
            MultiStateDB::MDBXState(db) => db.read_storage(address, key),
        }
    }

    fn read_latest_block_hash(&self) -> Result<H256, StorageError> {
        match self {
            MultiStateDB::RocksDBState(db) => db.read_latest_block_hash(),
            MultiStateDB::RocksDBArchive(db) => db.read_latest_block_hash(),
            MultiStateDB::MDBXState(db) => db.read_latest_block_hash(),
        }
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<Block<H256>>, StorageError> {
        match self {
            MultiStateDB::RocksDBState(db) => db.read_block_info(block_hash),
            MultiStateDB::RocksDBArchive(db) => db.read_block_info(block_hash),
            MultiStateDB::MDBXState(db) => db.read_block_info(block_hash),
        }
    }

    fn read_block_hash(&self, block_num: u64) -> Result<H256, StorageError> {
        match self {
            MultiStateDB::RocksDBState(db) => db.read_block_hash(block_num),
            MultiStateDB::RocksDBArchive(db) => db.read_block_hash(block_num),
            MultiStateDB::MDBXState(db) => db.read_block_hash(block_num),
        }
    }
}

pub enum MultiWriteBatch {
    RocksDBBatch(rocksdb::WriteBatch),
    MDBXBatch(MDBXWriteBatch),
}

impl StateDBWrite for MultiStateDB {
    type DBWriteBatch = MultiWriteBatch;

    fn prepare_write_batch(&self) -> Result<Self::DBWriteBatch, StorageError> {
        match self {
            MultiStateDB::RocksDBState(db) => {
                Ok(MultiWriteBatch::RocksDBBatch(db.prepare_write_batch()?))
            }
            MultiStateDB::RocksDBArchive(db) => {
                Ok(MultiWriteBatch::RocksDBBatch(db.prepare_write_batch()?))
            }
            MultiStateDB::MDBXState(db) => {
                Ok(MultiWriteBatch::MDBXBatch(db.prepare_write_batch()?))
            }
        }
    }

    fn write_latest_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_hash: H256,
    ) -> Result<(), StorageError> {
        match (self, batch) {
            (MultiStateDB::RocksDBState(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_latest_block_hash(b, block_hash)
            }
            (MultiStateDB::RocksDBArchive(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_latest_block_hash(b, block_hash)
            }
            (MultiStateDB::MDBXState(db), MultiWriteBatch::MDBXBatch(b)) => {
                db.write_latest_block_hash(b, block_hash)
            }
            _ => Err(StorageError::UnSupported("Batch type mismatch".to_string())),
        }
    }

    fn write_block_info(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_info: Block<H256>,
    ) -> Result<(), StorageError> {
        match (self, batch) {
            (MultiStateDB::RocksDBState(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_block_info(b, block_info)
            }
            (MultiStateDB::RocksDBArchive(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_block_info(b, block_info)
            }
            (MultiStateDB::MDBXState(db), MultiWriteBatch::MDBXBatch(b)) => {
                db.write_block_info(b, block_info)
            }
            _ => Err(StorageError::UnSupported("Batch type mismatch".to_string())),
        }
    }

    fn write_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_num: u64,
        block_hash: H256,
    ) -> Result<(), StorageError> {
        match (self, batch) {
            (MultiStateDB::RocksDBState(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_block_hash(b, block_num, block_hash)
            }
            (MultiStateDB::RocksDBArchive(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_block_hash(b, block_num, block_hash)
            }
            (MultiStateDB::MDBXState(db), MultiWriteBatch::MDBXBatch(b)) => {
                db.write_block_hash(b, block_num, block_hash)
            }
            _ => Err(StorageError::UnSupported("Batch type mismatch".to_string())),
        }
    }

    fn write_account(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        block_num: u64,
        raw_account: Option<NewAccount>,
    ) -> Result<(), StorageError> {
        match (self, batch) {
            (MultiStateDB::RocksDBState(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_account(b, address, block_num, raw_account)
            }
            (MultiStateDB::RocksDBArchive(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_account(b, address, block_num, raw_account)
            }
            (MultiStateDB::MDBXState(db), MultiWriteBatch::MDBXBatch(b)) => {
                db.write_account(b, address, block_num, raw_account)
            }
            _ => Err(StorageError::UnSupported("Batch type mismatch".to_string())),
        }
    }

    fn write_code(
        &self,
        batch: &mut Self::DBWriteBatch,
        code_hash: H256,
        code: Bytes,
    ) -> Result<(), StorageError> {
        match (self, batch) {
            (MultiStateDB::RocksDBState(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_code(b, code_hash, code)
            }
            (MultiStateDB::RocksDBArchive(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_code(b, code_hash, code)
            }
            (MultiStateDB::MDBXState(db), MultiWriteBatch::MDBXBatch(b)) => {
                db.write_code(b, code_hash, code)
            }
            _ => Err(StorageError::UnSupported("Batch type mismatch".to_string())),
        }
    }

    fn write_storage(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        key: H256,
        block_num: u64,
        value: U256,
    ) -> Result<(), StorageError> {
        match (self, batch) {
            (MultiStateDB::RocksDBState(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_storage(b, address, key, block_num, value)
            }
            (MultiStateDB::RocksDBArchive(db), MultiWriteBatch::RocksDBBatch(b)) => {
                db.write_storage(b, address, key, block_num, value)
            }
            (MultiStateDB::MDBXState(db), MultiWriteBatch::MDBXBatch(b)) => {
                db.write_storage(b, address, key, block_num, value)
            }
            _ => Err(StorageError::UnSupported("Batch type mismatch".to_string())),
        }
    }

    fn commit(&self, batch: Self::DBWriteBatch) -> Result<(), StorageError> {
        match (self, batch) {
            (MultiStateDB::RocksDBState(db), MultiWriteBatch::RocksDBBatch(b)) => db.commit(b),
            (MultiStateDB::RocksDBArchive(db), MultiWriteBatch::RocksDBBatch(b)) => db.commit(b),
            (MultiStateDB::MDBXState(db), MultiWriteBatch::MDBXBatch(b)) => db.commit(b),
            _ => Err(StorageError::UnSupported("Batch type mismatch".to_string())),
        }
    }
}
