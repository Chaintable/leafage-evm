use leafage_evm_types::BlockId;
use revm::database_interface::DBErrorMarker;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error<DBError> {
    #[error("DB error: {0}")]
    DBError(DBError),
    #[error("Parent Block not found")]
    ParentBlockHashNotFound,
    #[error("BlockId: {0:?} not supported")]
    UnsupportedBlockId(BlockId),
}

impl<DBError> From<DBError> for Error<DBError> {
    fn from(e: DBError) -> Self {
        Error::DBError(e)
    }
}

impl<DBError> DBErrorMarker for Error<DBError> {}
