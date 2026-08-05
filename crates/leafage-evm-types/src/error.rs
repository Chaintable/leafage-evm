pub type BundleStorageDiffResult<T> = std::result::Result<T, BundleStorageDiffError>;

#[derive(Debug, thiserror::Error)]
pub enum BundleStorageDiffError {
    #[error("StateDiff bundle is shorter than its {index_size}-byte index: got {actual}")]
    BundleTooShort { index_size: usize, actual: usize },

    #[error("StateDiff index must be {expected} bytes, got {actual}")]
    InvalidIndexLength { expected: usize, actual: usize },

    #[error("StateDiff payload must be {expected} bytes, got {actual}")]
    PayloadLengthMismatch { expected: usize, actual: usize },

    #[error("first StateDiff offset must be 0, got {actual}")]
    NonZeroFirstOffset { actual: u64 },

    #[error("StateDiff offsets must increase at entry {position}: {next} <= {current}")]
    NonIncreasingOffset {
        position: usize,
        current: u64,
        next: u64,
    },

    #[error("StateDiff offset {offset} at position {position} does not fit usize")]
    OffsetOverflow { position: usize, offset: u64 },

    #[error("unused StateDiff offset {position} must be 0, got {actual}")]
    NonZeroUnusedOffset { position: usize, actual: u64 },

    #[error("unused StateDiff bitmap bit {position} must be 0")]
    NonZeroUnusedBitmap { position: usize },

    #[error("StateDiff position {position} is outside bundle capacity {capacity}")]
    PositionOutOfRange { position: usize, capacity: usize },

    #[error("decode StateDiff entry {position}: {source}")]
    Rlp {
        position: usize,
        #[source]
        source: alloy_rlp::Error,
    },
}
