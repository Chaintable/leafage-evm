//! Offline archive truncation. Delete future versions in bounded batches and
//! publish the target head only after every table has been processed.
use super::{set_inverted_block_encoding, MultiStorage, StorageError, StorageKind};
use leafage_evm_types::{BlockInfo, H256};
use std::path::Path;

pub(crate) const REWIND_BATCH_BYTES: usize = 1024 * 1024;
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

/// Validate keys before interpreting them. Malformed data stops truncation
/// rather than silently publishing a partially truncated database.
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
            super::rocksdb_impl::decode_archive_header(&mut &value[..])?.number
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

/// Reset the offset before deleting archive records and persist the removal.
fn reset_offset(file: &Path) -> Result<(), StorageError> {
    let absolute = if file.is_absolute() {
        file.to_path_buf()
    } else {
        std::env::current_dir()?.join(file)
    };
    match std::fs::remove_file(&absolute) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    // Sync even when the file is absent. A missing directory uses its nearest
    // existing ancestor; no directories are created for offset cleanup.
    let mut parent = absolute.parent().unwrap();
    loop {
        match std::fs::File::open(parent) {
            Ok(dir) => {
                dir.sync_all()?;
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                parent = parent.parent().ok_or(e)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

impl MultiStorage {
    /// Exclusive maintenance open: never create missing databases or tables.
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
                super::ArchiveRocksDBStorage::open_for_rewind(path, cache_size)?,
            )),
            StorageKind::MDBX => Self::MDBXArchive(std::sync::Arc::new(
                super::MDBXArchiveStorage::open_for_rewind(path)?,
            )),
        })
    }

    fn rewind_block(&self, number: Option<u64>) -> Result<BlockInfo, StorageError> {
        let (hash, block) = match self {
            Self::RocksDBArchive(db) => {
                let hash = match number {
                    Some(n) => db.read_block_hash(n)?,
                    None => db.read_latest_block_hash()?,
                };
                (hash, db.read_block_info(hash)?)
            }
            Self::MDBXArchive(db) => {
                let hash = match number {
                    Some(n) => db.read_block_hash(n)?,
                    None => db.read_latest_block_hash()?,
                };
                (hash, db.read_block_info(hash)?)
            }
            _ => {
                return Err(StorageError::UnSupported(
                    "real rewind requires archive mode".into(),
                ))
            }
        };
        let block = block.ok_or_else(|| {
            StorageError::UnSupported(format!("archive block {number:?} not found"))
        })?;
        if hash == H256::ZERO
            || hash != block.header.hash
            || number.is_some_and(|n| n != block.header.number)
        {
            return Err(StorageError::UnSupported(
                "inconsistent archive block index/header".into(),
            ));
        }
        Ok(block)
    }

    /// Offline operation: precheck, reset offset, delete future records, publish H.
    /// Batches are durable; the whole operation has no atomicity or recovery guarantee.
    /// All database users must remain stopped until this method succeeds.
    pub fn rewind_archive(
        &self,
        number: u64,
        encoding: Option<bool>,
        offset_file: &Path,
    ) -> Result<BlockInfo, StorageError> {
        let (disk_encoding, populated) = match self {
            Self::RocksDBArchive(db) => db.rewind_layout()?,
            Self::MDBXArchive(db) => db.rewind_layout()?,
            _ => {
                return Err(StorageError::UnSupported(
                    "real rewind requires archive mode".into(),
                ))
            }
        };
        let stored_encoding = match disk_encoding.as_deref() {
            None => None,
            Some([0]) => Some(false),
            Some([1]) => Some(true),
            _ => {
                return Err(StorageError::UnSupported(
                    "invalid archive encoding marker".into(),
                ))
            }
        };
        if stored_encoding.zip(encoding).is_some_and(|(a, b)| a != b) {
            return Err(StorageError::UnSupported(
                "--archive-encoding conflicts with stored encoding".into(),
            ));
        }
        let inverted = stored_encoding.or(encoding).ok_or_else(|| {
            StorageError::UnSupported(
                "unmarked archive requires --archive-encoding legacy|inverted".into(),
            )
        })?;
        if !populated && stored_encoding.is_none() {
            return Err(StorageError::UnSupported(
                "cannot identify an empty unmarked database as archive".into(),
            ));
        }
        let head = self.rewind_block(None)?;
        if number > head.header.number {
            return Err(StorageError::UnSupported(
                "rewind target is above the committed head".into(),
            ));
        }
        let target = self.rewind_block(Some(number))?;
        reset_offset(offset_file)?;
        set_inverted_block_encoding(inverted);
        match self {
            Self::RocksDBArchive(db) => db.rewind_to(&target, inverted)?,
            Self::MDBXArchive(db) => db.rewind_to(&target, inverted)?,
            _ => unreachable!(),
        }
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_height_inverted_is_not_a_legacy_sentinel() {
        for (table, len) in [(RewindTable::Accounts, 64), (RewindTable::Storage, 96)] {
            let mut key = vec![0; len];
            key[len - 8..].copy_from_slice(&u64::MAX.to_be_bytes());
            assert!(!remove_record(table, &key, &[], 0, true, false).unwrap());
            assert!(remove_record(table, &key, &[], 0, false, false).unwrap());
            key[len - 32] = 1;
            assert!(remove_record(table, &key, &[], 0, true, false).is_err());
        }
    }

    #[test]
    fn empty_unmarked_archives_are_not_identifiable() {
        use crate::{EvmStorageWrite, StateDBProvider, StateDBWrapper};
        use leafage_evm_types::{Block, BlockId, BlockStorageDiff, Header, RawHeader};
        let _lock = crate::db_impl::rocksdb_impl::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_inverted_block_encoding(false);
        for kind in [StorageKind::Rocksdb, StorageKind::MDBX] {
            let dir =
                std::env::temp_dir().join(format!("empty-rewind-{kind:?}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let db = MultiStorage::open(&dir, 16, kind, true, false, false).unwrap();
            StateDBWrapper(db.db_at(BlockId::latest()).unwrap().unwrap())
                .update_block(
                    BlockInfo::new(Block {
                        header: Header {
                            hash: H256::repeat_byte(1),
                            inner: RawHeader {
                                number: 1,
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
                    BlockStorageDiff::default(),
                )
                .unwrap();
            assert!(db
                .rewind_archive(1, Some(false), &dir.join("offset/offset"))
                .unwrap_err()
                .to_string()
                .contains("empty unmarked"));
            drop(db);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
