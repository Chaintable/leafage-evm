mod snapshot;
pub use snapshot::DataBase as RocksDBStorage;

mod archive;
pub(crate) use archive::decode_archive_header;
pub use archive::{DataBaseRef as ArchiveRocksDBStorage, StateDB as ArchiveStateDB};

#[cfg(test)]
pub(crate) use archive::ARCHIVE_DB_TEST_LOCK;
