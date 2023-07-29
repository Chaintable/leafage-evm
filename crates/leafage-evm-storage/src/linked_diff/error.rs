pub enum Error<DBError> {
    DBError(DBError),
    ParentBlockHashNotFound,
}

impl<DBError> From<DBError> for Error<DBError> {
    fn from(e: DBError) -> Self {
        Error::DBError(e)
    }
}
