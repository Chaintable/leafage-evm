mod snapshot;
pub use snapshot::DataBase as RocksDBStorage;

mod archive;
pub use archive::{DataBaseRef as ArchiveRocksDBStorage, StateDB as ArchiveStateDB};

#[cfg(test)]
pub(crate) use archive::ARCHIVE_DB_TEST_LOCK;
