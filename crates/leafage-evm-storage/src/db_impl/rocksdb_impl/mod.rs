mod snapshot;
pub use snapshot::DataBase as RocksDBStorage;

mod archive;
#[cfg(test)]
pub(crate) use archive::ARCHIVE_DB_TEST_LOCK;
pub use archive::{DataBaseRef as ArchiveRocksDBStorage, StateDB as ArchiveStateDB};
