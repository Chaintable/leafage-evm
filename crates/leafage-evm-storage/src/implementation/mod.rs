mod scheme;
pub use scheme::{StateDBRead, StateDBWrite};

#[cfg(feature = "with-rocksdb")]
mod rocksdb_impl;

#[cfg(feature = "with-rocksdb")]
pub use rocksdb_impl::DataBase as RocksDBStorage;
