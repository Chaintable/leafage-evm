//! Database implementation for EVM storage.

#[cfg(feature = "with-rocksdb")]
mod rocksdb_impl;
#[cfg(feature = "with-rocksdb")]
pub use rocksdb_impl::DataBase as RocksDBStorage;
