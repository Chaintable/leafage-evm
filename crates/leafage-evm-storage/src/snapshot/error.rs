use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error<DBError> {
    #[error("DB error: {0}")]
    DBError(DBError),
    #[error("Block not found")]
    ParentBlockHashNotFound,
}

impl<DBError> From<DBError> for Error<DBError> {
    fn from(e: DBError) -> Self {
        Error::DBError(e)
    }
}
