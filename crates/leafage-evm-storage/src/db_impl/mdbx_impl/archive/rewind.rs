use super::*;
use crate::db_impl::rewind::{
    remove_record, RewindTable, REWIND_BATCH_SIZE, REWIND_KEY, REWIND_TABLES,
};

fn mdbx_error(error: libmdbx::Error) -> Error {
    Error::UnSupported(format!("MDBX archive rewind: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        set_inverted_block_encoding, EvmStorageWrite, MultiStorage, StateDBWrapper, StorageKind,
    };
    use leafage_evm_types::{Block, BlockStorageDiff, Header, RawHeader};

    #[test]
    fn mdbx_rewind_recovers_after_reopening_exclusively() {
        let _lock = crate::db_impl::rocksdb_impl::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_inverted_block_encoding(false);
        let dir = std::env::temp_dir().join(format!("mdbx-rewind-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = Arc::new(DataBase::open(&dir));
        let db = MultiStorage::MDBXArchive(raw.clone());
        for number in 1..=2 {
            let mut header = RawHeader::default();
            header.number = number;
            header.parent_hash = H256::repeat_byte(number as u8 - 1);
            let block = BlockInfo::new(Block {
                header: Header {
                    hash: H256::repeat_byte(number as u8),
                    inner: header,
                    ..Default::default()
                },
                ..Default::default()
            });
            StateDBWrapper(db.db_at(BlockId::latest()).unwrap().unwrap())
                .update_block(
                    block,
                    BlockStorageDiff {
                        new_accounts: vec![NewAccount {
                            address: H256::repeat_byte(10),
                            balance: U256::from(number),
                            nonce: number,
                            code_hash: KECCAK256_EMPTY,
                        }],
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        let txn = raw.env.begin_rw_txn().unwrap();
        txn.put(
            raw.dbis[StorageTable::AddressToStorage.to_str()],
            &[0u8],
            &[0u8],
            WriteFlags::empty(),
        )
        .unwrap();
        txn.commit().unwrap();
        assert!(db.rewind_archive(1).is_err());
        assert_eq!(raw.read_latest_block_hash().unwrap(), H256::repeat_byte(2));
        drop(db);
        drop(raw);
        assert!(MultiStorage::open(&dir, 16, StorageKind::MDBX, true, false, false).is_err());
        let db = MultiStorage::open_for_archive_rewind(&dir, 16, StorageKind::MDBX).unwrap();
        let MultiStorage::MDBXArchive(raw) = &db else {
            unreachable!()
        };
        let txn = raw.env.begin_rw_txn().unwrap();
        txn.del(
            raw.dbis[StorageTable::AddressToStorage.to_str()],
            &[0u8],
            None,
        )
        .unwrap();
        txn.commit().unwrap();
        db.rewind_archive(1).unwrap();
        drop(db);
        let db = MultiStorage::open(&dir, 16, StorageKind::MDBX, true, false, false).unwrap();
        let state = db.db_at(BlockId::latest()).unwrap().unwrap();
        assert_eq!(
            state.read_latest_block_hash().unwrap(),
            H256::repeat_byte(1)
        );
        assert_eq!(
            state
                .read_account(H256::repeat_byte(10))
                .unwrap()
                .unwrap()
                .balance,
            U256::from(1)
        );
        assert!(db.db_at(BlockId::number(2)).unwrap().is_none());
        drop(state);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

impl DataBase {
    pub(crate) fn rewind_marker(&self) -> Result<Option<Vec<u8>>, Error> {
        let txn = self.env.begin_ro_txn().map_err(mdbx_error)?;
        txn.get(
            self.dbis[StorageTable::LatestBlockHash.to_str()],
            REWIND_KEY,
        )
        .map_err(mdbx_error)
    }

    pub(crate) fn rewind_to(&self, target: &BlockInfo) -> Result<(), Error> {
        let meta = self.dbis[StorageTable::LatestBlockHash.to_str()];
        let marker = serde_json::to_vec(&(target, inverted_block_encoding()))?;
        let txn = self.env.begin_rw_txn().map_err(mdbx_error)?;
        txn.put(meta, REWIND_KEY, &marker, WriteFlags::empty())
            .map_err(mdbx_error)?;
        txn.commit().map_err(mdbx_error)?;
        self.sync(true)?;

        for table in REWIND_TABLES {
            let name = match table {
                RewindTable::Accounts => StorageTable::AddressToAccount,
                RewindTable::Storage => StorageTable::AddressToStorage,
                RewindTable::BlockNumbers => StorageTable::BlockNumToBlockHash,
                RewindTable::Headers => StorageTable::BlockHashToBlockInfo,
            }
            .to_str();
            let mut after: Option<Vec<u8>> = None;
            let mut deleted = 0u64;
            loop {
                // Release the read transaction before deleting a chunk, so a
                // full archive scan does not pin every old MDBX page.
                let (last, keys) = {
                    let txn = self.env.begin_ro_txn().map_err(mdbx_error)?;
                    let db = txn.open_db(Some(name)).map_err(mdbx_error)?;
                    let mut cursor = txn.cursor(&db).map_err(mdbx_error)?;
                    let mut row = match &after {
                        Some(key) => cursor
                            .set_range::<Vec<u8>, Vec<u8>>(key)
                            .map_err(mdbx_error)?,
                        None => cursor.first::<Vec<u8>, Vec<u8>>().map_err(mdbx_error)?,
                    };
                    if let (Some(key), Some((found, _))) = (&after, &row) {
                        if found == key {
                            row = cursor.next::<Vec<u8>, Vec<u8>>().map_err(mdbx_error)?;
                        }
                    }
                    let mut keys = Vec::new();
                    let mut last = None;
                    for _ in 0..REWIND_BATCH_SIZE {
                        let Some((key, value)) = row else { break };
                        if remove_record(
                            table,
                            &key,
                            &value,
                            target.header.number,
                            inverted_block_encoding(),
                            true,
                        )? {
                            keys.push(key.clone());
                        }
                        last = Some(key);
                        row = cursor.next::<Vec<u8>, Vec<u8>>().map_err(mdbx_error)?;
                    }
                    (last, keys)
                };
                let Some(last) = last else { break };
                if !keys.is_empty() {
                    let txn = self.env.begin_rw_txn().map_err(mdbx_error)?;
                    for key in &keys {
                        txn.del(self.dbis[name], key, None).map_err(mdbx_error)?;
                    }
                    txn.commit().map_err(mdbx_error)?;
                    self.sync(true)?;
                    deleted += keys.len() as u64;
                }
                after = Some(last);
            }
            info!(target: "rewind", ?table, deleted, "archive table truncated");
        }
        let txn = self.env.begin_rw_txn().map_err(mdbx_error)?;
        txn.put(
            meta,
            LATEST_BLOCK_HASH_KEY,
            target.header.hash.as_slice(),
            WriteFlags::empty(),
        )
        .map_err(mdbx_error)?;
        txn.del(meta, REWIND_KEY, None).map_err(mdbx_error)?;
        txn.commit().map_err(mdbx_error)?;
        self.sync(true)?;
        Ok(())
    }
}
