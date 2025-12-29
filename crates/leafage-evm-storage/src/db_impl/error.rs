use alloy::rpc::types::ConversionError;
use leafage_evm_types::BlockId;
use revm::database_interface::DBErrorMarker;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("rocksdb error, {0}")]
    RocksDB(#[from] rocksdb::Error),
    #[error("rlp error, {0}")]
    Rlp(#[from] alloy_rlp::Error),
    #[error("unsupported operation, {0}")]
    UnSupported(String),
    #[error("unsupported block id, {0}")]
    UnsupportedBlockId(BlockId),
    #[error("conversion error, {0}")]
    Conversion(#[from] ConversionError),
    #[error("serde_json error, {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("iterator timed out for block {0}")]
    IteratorTimedOut(u64),
}

impl DBErrorMarker for Error {}
