//! Offline archive truncation. A durable marker prevents normal startup after
//! a partial deletion; retrying the same target rescans the tables idempotently.
use super::{inverted_block_encoding, MultiStorage, StorageError, StorageKind};
use alloy_rlp::Decodable;
use leafage_evm_types::{BlockInfo, RawHeader, H256};
use std::path::Path;

pub(crate) const REWIND_KEY: &[u8] = b"leafage:archive_rewind";
pub(crate) const REWIND_BATCH_SIZE: usize = if cfg!(test) { 2 } else { 10_000 };

#[derive(Clone, Copy, Debug)]
pub(crate) enum RewindTable {
    Accounts,
    Storage,
    BlockNumbers,
    Headers,
}

pub(crate) const REWIND_TABLES: [RewindTable; 4] = [
    RewindTable::Accounts,
    RewindTable::Storage,
    RewindTable::BlockNumbers,
    RewindTable::Headers,
];

/// Validate keys before interpreting them. Malformed data leaves the marker in
/// place rather than silently publishing a partially truncated database.
pub(crate) fn remove_record(
    table: RewindTable,
    key: &[u8],
    value: &[u8],
    target: u64,
    inverted: bool,
    json_headers: bool,
) -> Result<bool, StorageError> {
    let key_len = match table {
        RewindTable::Accounts => 64,
        RewindTable::Storage => 96,
        _ => 32,
    };
    if key.len() != key_len {
        return Err(StorageError::UnSupported(format!(
            "invalid {table:?} key length {} during archive rewind",
            key.len()
        )));
    }
    if matches!(table, RewindTable::Headers) {
        let number = if json_headers {
            serde_json::from_slice::<BlockInfo>(value)?.header.number
        } else {
            RawHeader::decode(&mut &value[..])?.number
        };
        return Ok(number > target);
    }
    let tail = &key[key_len - 32..];
    if tail[..24] != [0; 24] {
        return Err(StorageError::UnSupported(
            "invalid archive block height".into(),
        ));
    }
    let raw = u64::from_be_bytes(tail[24..].try_into().unwrap());
    let versioned = matches!(table, RewindTable::Accounts | RewindTable::Storage);
    // Legacy dual-write latest pointers must not survive a real rewind.
    if versioned && !inverted && raw == u64::MAX {
        return Ok(true);
    }
    let height = if versioned && inverted {
        u64::MAX - raw
    } else {
        raw
    };
    Ok(height > target)
}

impl MultiStorage {
    /// Opens an existing archive exclusively for maintenance. Normal callers
    /// must use `open`, which refuses an unfinished rewind.
    pub fn open_for_archive_rewind(
        path: &Path,
        cache_size: usize,
        kind: StorageKind,
    ) -> Result<Self, StorageError> {
        let data_file = match kind {
            StorageKind::Rocksdb => "CURRENT",
            StorageKind::MDBX => "mdbx.dat",
        };
        if !path.join(data_file).is_file() {
            return Err(StorageError::UnSupported(format!(
                "archive database does not exist at {}",
                path.display()
            )));
        }
        Ok(match kind {
            StorageKind::Rocksdb => Self::RocksDBArchive(std::sync::Arc::new(
                super::ArchiveRocksDBStorage::open(path, cache_size, false, false),
            )),
            StorageKind::MDBX => Self::MDBXArchive(std::sync::Arc::new(
                super::MDBXArchiveStorage::open_for_rewind(path),
            )),
        })
    }

    fn rewind_marker(&self) -> Result<Option<Vec<u8>>, StorageError> {
        match self {
            Self::RocksDBArchive(db) => db.rewind_marker(),
            Self::MDBXArchive(db) => db.rewind_marker(),
            _ => Ok(None),
        }
    }

    pub(crate) fn ensure_no_rewind(&self) -> Result<(), StorageError> {
        if self.rewind_marker()?.is_some() {
            return Err(StorageError::UnSupported(
                "archive rewind is incomplete; rerun rewind --archive with the same target before starting the node".into(),
            ));
        }
        Ok(())
    }

    /// Resolve and validate before the CLI invalidates its Kafka offset.
    pub fn archive_rewind_target(&self, number: u64) -> Result<BlockInfo, StorageError> {
        if let Some(marker) = self.rewind_marker()? {
            let (target, inverted): (BlockInfo, bool) = serde_json::from_slice(&marker)?;
            if target.header.number != number || inverted != inverted_block_encoding() {
                return Err(StorageError::UnSupported(format!(
                    "unfinished rewind targets block {} with inverted encoding {}; retry with the same target and encoding",
                    target.header.number, inverted
                )));
            }
            return Ok(target);
        }
        let (head, target) = match self {
            Self::RocksDBArchive(db) => (
                db.read_block_info(db.read_latest_block_hash()?)?,
                db.read_block_info(db.read_block_hash(number)?)?,
            ),
            Self::MDBXArchive(db) => (
                db.read_block_info(db.read_latest_block_hash()?)?,
                db.read_block_info(db.read_block_hash(number)?)?,
            ),
            _ => {
                return Err(StorageError::UnSupported(
                    "real rewind requires archive mode".into(),
                ))
            }
        };
        let head =
            head.ok_or_else(|| StorageError::UnSupported("archive has no committed head".into()))?;
        let target = target.ok_or_else(|| {
            StorageError::UnSupported(format!("archive block {number} not found"))
        })?;
        // Equality also allows repairing an archive previously rewound by the
        // old pointer-only command, and makes successful retries harmless.
        if number > head.header.number
            || target.header.number != number
            || target.header.hash == H256::ZERO
        {
            return Err(StorageError::UnSupported(
                "invalid archive rewind target".into(),
            ));
        }
        Ok(target)
    }

    /// Delete future state and block indexes, then atomically publish the head.
    /// Callers must stop the node and durably reset its Kafka offset first.
    pub fn rewind_archive(&self, number: u64) -> Result<BlockInfo, StorageError> {
        let target = self.archive_rewind_target(number)?;
        match self {
            Self::RocksDBArchive(db) => db.rewind_to(&target)?,
            Self::MDBXArchive(db) => db.rewind_to(&target)?,
            _ => unreachable!("archive_rewind_target rejects state databases"),
        }
        Ok(target)
    }
}
