mod snapshot;
pub use snapshot::DataBase as RocksDBStorage;

mod archive;
pub use archive::{DataBase as ArchiveRocksDBStorage, StateDB as ArchiveStateDB};
