//! Database implementation for EVM storage.

#[cfg(feature = "with-rocksdb")]
mod rocksdb_impl;
#[cfg(feature = "with-rocksdb")]
pub use rocksdb_impl::DataBase as RocksDBStorage;

#[cfg(feature = "with-rocksdb")]
mod archive_rocksdb_db_impl;
#[cfg(feature = "with-rocksdb")]
pub use archive_rocksdb_db_impl::{DataBase as ArchiveRocksDBStorage, StateDB as ArchiveStateDB};
