use leafage_evm_types::{BlockId, H256};
use revm::database_interface::DBErrorMarker;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error<DBError> {
    #[error("DB error: {0}")]
    DBError(DBError),
    #[error("Parent block hash not found: {0:?} at block number {1}, expected parent hash {2}")]
    ParentBlockHashNotFound(H256, u64, H256),
    #[error("BlockId: {0:?} not supported")]
    UnsupportedBlockId(BlockId),
    #[error("No latest block found in DB")]
    NoLatestBlockInDB,
}

impl<DBError> From<DBError> for Error<DBError> {
    fn from(e: DBError) -> Self {
        Error::DBError(e)
    }
}

impl<DBError: Send + Sync + std::fmt::Debug + std::fmt::Display + 'static> DBErrorMarker for Error<DBError> {}
