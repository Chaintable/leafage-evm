use super::*;
use crate::db_impl::rewind::{
    remove_record, RewindTable, REWIND_BATCH_SIZE, REWIND_KEY, REWIND_TABLES,
};

impl DataBaseRef {
    pub(crate) fn rewind_marker(&self) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.db.get_cf(
            self.db
                .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
                .unwrap(),
            REWIND_KEY,
        )?)
    }

    pub(crate) fn rewind_to(&self, target: &BlockInfo) -> Result<(), Error> {
        let meta = self
            .db
            .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
            .unwrap();
        let mut options = WriteOptions::default();
        options.set_sync(true);
        let marker = serde_json::to_vec(&(target, inverted_block_encoding()))?;
        self.db.put_cf_opt(meta, REWIND_KEY, marker, &options)?;

        for table in REWIND_TABLES {
            let name = match table {
                RewindTable::Accounts => StorageTypeColumn::AddressToAccount,
                RewindTable::Storage => StorageTypeColumn::AddressToStorage,
                RewindTable::BlockNumbers => StorageTypeColumn::BlockNumToBlockHash,
                RewindTable::Headers => StorageTypeColumn::BlockHashToBlockInfo,
            };
            let cf = self.db.cf_handle(name.to_str()).unwrap();
            let mut read_options = ReadOptions::default();
            read_options.set_total_order_seek(true);
            read_options.fill_cache(false);
            let mut batch = WriteBatch::default();
            let mut deleted = 0u64;
            for item in self
                .db
                .iterator_cf_opt(cf, read_options, IteratorMode::Start)
            {
                let (key, value) = item?;
                if remove_record(
                    table,
                    &key,
                    &value,
                    target.header.number,
                    inverted_block_encoding(),
                    false,
                )? {
                    batch.delete_cf(cf, key);
                    deleted += 1;
                    if batch.len() >= REWIND_BATCH_SIZE {
                        self.db.write_opt(batch, &options)?;
                        batch = WriteBatch::default();
                        info!(target: "rewind", ?table, deleted, "truncating archive");
                    }
                }
            }
            self.db.write_opt(batch, &options)?;
            info!(target: "rewind", ?table, deleted, "archive table truncated");
        }

        // Keep all content-addressed code. Only reachable account code hashes
        // affect state; collecting unused code is independent of rewind.
        let mut batch = WriteBatch::default();
        batch.put_cf(meta, [1u8], target.header.hash.as_slice());
        batch.delete_cf(meta, REWIND_KEY);
        self.db.write_opt(batch, &options)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EvmStorageWrite, LatestStateDBIterator, MultiStorage, StateDBWrapper, StorageKind,
    };
    use leafage_evm_types::{AccountStorageDiff, BlockStorageDiff, IndexValuePair, NewCode};

    fn block(number: u64, hash: u8, parent: u8) -> BlockInfo {
        let mut header = RawHeader::default();
        header.number = number;
        header.parent_hash = H256::repeat_byte(parent);
        BlockInfo::new(Block {
            header: Header {
                hash: H256::repeat_byte(hash),
                inner: header,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn account(address: u8, balance: u64) -> NewAccount {
        NewAccount {
            address: H256::repeat_byte(address),
            balance: U256::from(balance),
            nonce: balance,
            code_hash: KECCAK256_EMPTY.0.into(),
        }
    }

    fn slots(address: u8, values: &[(u8, u64)]) -> AccountStorageDiff {
        AccountStorageDiff {
            address: H256::repeat_byte(address),
            diffs: values
                .iter()
                .map(|(slot, value)| IndexValuePair {
                    index: H256::repeat_byte(*slot),
                    value: U256::from(*value),
                })
                .collect(),
        }
    }

    fn commit(db: &MultiStorage, block: BlockInfo, diff: BlockStorageDiff) {
        StateDBWrapper(db.db_at(BlockId::latest()).unwrap().unwrap())
            .update_block(block, diff)
            .unwrap();
    }

    fn exercise(kind: StorageKind, inverted: bool) {
        let _lock = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_inverted_block_encoding(inverted);
        let dir = std::env::temp_dir().join(format!(
            "archive-real-rewind-{kind:?}-{inverted}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = MultiStorage::open(&dir, 16, kind, true, false, false).unwrap();
        let code_hash = H256::repeat_byte(0xee);
        commit(
            &db,
            block(1, 1, 0),
            BlockStorageDiff {
                new_accounts: vec![account(10, 5), account(11, 6), account(12, 7)],
                storage_diffs: vec![slots(10, &[(1, 5), (2, 6)])],
                ..Default::default()
            },
        );
        // H already contains a deletion and zero. Rewind must preserve those
        // tombstones, not resurrect the preceding nonempty version.
        commit(
            &db,
            block(2, 2, 1),
            BlockStorageDiff {
                deleted_accounts: vec![H256::repeat_byte(12)],
                storage_diffs: vec![slots(10, &[(2, 0)])],
                ..Default::default()
            },
        );
        let mut changed = account(10, 9);
        changed.code_hash = code_hash;
        commit(
            &db,
            block(3, 3, 2),
            BlockStorageDiff {
                new_accounts: vec![changed, account(12, 9), account(13, 9)],
                deleted_accounts: vec![H256::repeat_byte(11)],
                storage_diffs: vec![slots(10, &[(1, 0), (2, 9), (3, 9)])],
                new_codes: vec![NewCode {
                    code_hash,
                    code: vec![0x00].into(),
                }],
                ..Default::default()
            },
        );
        // An orphan header at the same height must also be removed, even when
        // the number index only points to the other hash.
        commit(&db, block(3, 4, 2), BlockStorageDiff::default());
        commit(&db, block(4, 5, 4), BlockStorageDiff::default());

        assert!(db.archive_rewind_target(0).is_err());
        assert!(db.archive_rewind_target(5).is_err());
        db.ensure_no_rewind().unwrap();
        db.rewind_archive(2).unwrap();
        db.ensure_no_rewind().unwrap();
        assert!(db.db_at(BlockId::number(3)).unwrap().is_none());
        for hash in [3, 4, 5] {
            assert!(db
                .db_at(BlockId::Hash(H256::repeat_byte(hash).into()))
                .unwrap()
                .is_none());
        }
        let latest = db.db_at(BlockId::latest()).unwrap().unwrap();
        assert_eq!(
            latest.read_latest_block_hash().unwrap(),
            H256::repeat_byte(2)
        );
        assert_eq!(
            latest
                .read_account(H256::repeat_byte(10))
                .unwrap()
                .unwrap()
                .balance,
            U256::from(5)
        );
        assert_eq!(
            latest
                .read_account(H256::repeat_byte(10))
                .unwrap()
                .unwrap()
                .code_hash,
            KECCAK256_EMPTY
        );
        assert!(latest
            .read_account(H256::repeat_byte(11))
            .unwrap()
            .is_some());
        assert!(latest
            .read_account(H256::repeat_byte(12))
            .unwrap()
            .is_none());
        assert!(latest
            .read_account(H256::repeat_byte(13))
            .unwrap()
            .is_none());
        assert_eq!(
            latest
                .read_storage(H256::repeat_byte(10), H256::repeat_byte(1))
                .unwrap(),
            U256::from(5)
        );
        for slot in [2, 3] {
            assert_eq!(
                latest
                    .read_storage(H256::repeat_byte(10), H256::repeat_byte(slot))
                    .unwrap(),
                U256::ZERO
            );
        }
        assert!(latest.read_code(code_hash).unwrap().is_some());
        drop(latest);
        let old = db.db_at(BlockId::number(1)).unwrap().unwrap();
        assert!(old.read_account(H256::repeat_byte(12)).unwrap().is_some());
        assert_eq!(
            old.read_storage(H256::repeat_byte(10), H256::repeat_byte(2))
                .unwrap(),
            U256::from(6)
        );
        drop(old);
        let accounts = db.account_iter().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(accounts.len(), 2);
        let storage = db.storage_iter().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(storage.len(), 1);

        // Retrying after success and after an old pointer-only rewind are both
        // permitted at target==head. The scan still removes physical futures.
        db.rewind_archive(2).unwrap();
        commit(&db, block(3, 6, 2), BlockStorageDiff::default());
        commit(&db, block(4, 7, 6), BlockStorageDiff::default());
        let new_head = db.db_at(BlockId::latest()).unwrap().unwrap();
        assert_eq!(
            new_head
                .read_account(H256::repeat_byte(10))
                .unwrap()
                .unwrap()
                .balance,
            U256::from(5)
        );
        assert!(new_head
            .read_account(H256::repeat_byte(11))
            .unwrap()
            .is_some());
        assert!(new_head
            .read_account(H256::repeat_byte(13))
            .unwrap()
            .is_none());
        assert_eq!(
            new_head
                .read_storage(H256::repeat_byte(10), H256::repeat_byte(1))
                .unwrap(),
            U256::from(5)
        );
        drop(new_head);
        drop(db);
        set_inverted_block_encoding(false);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rocksdb_legacy_archive_rewind() {
        exercise(StorageKind::Rocksdb, false);
    }
    #[test]
    fn rocksdb_inverted_archive_rewind() {
        exercise(StorageKind::Rocksdb, true);
    }
    #[test]
    fn mdbx_legacy_archive_rewind() {
        exercise(StorageKind::MDBX, false);
    }
    #[test]
    fn mdbx_inverted_archive_rewind() {
        exercise(StorageKind::MDBX, true);
    }

    #[test]
    fn interrupted_rewind_blocks_startup_and_resumes_after_head_header_deleted() {
        let _lock = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_inverted_block_encoding(false);
        let dir =
            std::env::temp_dir().join(format!("archive-rewind-resume-{}", std::process::id()));
        let raw = Arc::new(DataBaseRef::open(&dir, 16, false, false));
        let db = MultiStorage::RocksDBArchive(raw.clone());
        let target = block(1, 1, 0);
        commit(
            &db,
            target.clone(),
            BlockStorageDiff {
                new_accounts: vec![account(10, 5)],
                ..Default::default()
            },
        );
        commit(
            &db,
            block(2, 2, 1),
            BlockStorageDiff {
                new_accounts: vec![account(10, 9)],
                ..Default::default()
            },
        );
        // A corrupt storage key fails after account truncation has committed.
        // The command must preserve a durable marker and never publish H.
        raw.db
            .put_cf(raw.db.cf_handle("5").unwrap(), [0u8], [0u8])
            .unwrap();
        assert!(db.rewind_archive(1).is_err());
        assert_eq!(raw.read_latest_block_hash().unwrap(), H256::repeat_byte(2));
        assert!(raw
            .db
            .get_cf(
                raw.db.cf_handle("4").unwrap(),
                encode_account_key(H256::repeat_byte(10), 2)
            )
            .unwrap()
            .is_none());
        raw.db
            .delete_cf(raw.db.cf_handle("5").unwrap(), [0u8])
            .unwrap();
        // Model interruption after some deletion batches, including the old
        // head's header. Resume must rely on the durable target, not that head.
        raw.db
            .delete_cf(raw.db.cf_handle("2").unwrap(), H256::repeat_byte(2))
            .unwrap();
        assert!(db.ensure_no_rewind().is_err());
        assert!(db.archive_rewind_target(2).is_err());
        set_inverted_block_encoding(true);
        assert!(db.archive_rewind_target(1).is_err());
        set_inverted_block_encoding(false);
        drop(db);
        drop(raw);
        // Release the existing test-only singleton before reopening the same
        // database. Production rewind runs in a separate offline process.
        unsafe {
            super::super::DATA_BASE = None;
        }
        assert!(MultiStorage::open(&dir, 16, StorageKind::Rocksdb, true, false, false).is_err());
        unsafe {
            super::super::DATA_BASE = None;
        }
        let db = MultiStorage::open_for_archive_rewind(&dir, 16, StorageKind::Rocksdb).unwrap();
        db.rewind_archive(1).unwrap();
        db.ensure_no_rewind().unwrap();
        assert_eq!(
            db.db_at(BlockId::latest())
                .unwrap()
                .unwrap()
                .read_account(H256::repeat_byte(10))
                .unwrap()
                .unwrap()
                .balance,
            U256::from(5)
        );
        drop(db);
        unsafe {
            super::super::DATA_BASE = None;
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rewind_removes_legacy_sentinels_after_pointer_reset() {
        let _lock = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_inverted_block_encoding(false);
        let dir =
            std::env::temp_dir().join(format!("archive-rewind-sentinel-{}", std::process::id()));
        let raw = Arc::new(DataBaseRef::open(&dir, 16, false, false));
        let db = MultiStorage::RocksDBArchive(raw.clone());
        let target = block(1, 1, 0);
        commit(
            &db,
            target.clone(),
            BlockStorageDiff {
                new_accounts: vec![account(10, 5)],
                storage_diffs: vec![slots(10, &[(1, 5)])],
                ..Default::default()
            },
        );
        commit(
            &db,
            block(2, 2, 1),
            BlockStorageDiff {
                new_accounts: vec![account(10, 9)],
                storage_diffs: vec![slots(10, &[(1, 9)])],
                ..Default::default()
            },
        );
        let account_key = encode_account_key(H256::repeat_byte(10), u64::MAX);
        let storage_key = encode_storage_key(H256::repeat_byte(10), H256::repeat_byte(1), u64::MAX);
        raw.db
            .put_cf(
                raw.db.cf_handle("4").unwrap(),
                account_key,
                encode_slim_account(account(10, 9)),
            )
            .unwrap();
        raw.db
            .put_cf(
                raw.db.cf_handle("5").unwrap(),
                storage_key,
                U256::from(9).to_be_bytes::<32>(),
            )
            .unwrap();
        commit(&db, target, BlockStorageDiff::default());
        db.rewind_archive(1).unwrap();
        assert!(raw
            .db
            .get_cf(raw.db.cf_handle("4").unwrap(), account_key)
            .unwrap()
            .is_none());
        assert!(raw
            .db
            .get_cf(raw.db.cf_handle("5").unwrap(), storage_key)
            .unwrap()
            .is_none());
        assert_eq!(
            db.account_iter().next().unwrap().unwrap().1.balance,
            U256::from(5)
        );
        assert_eq!(db.storage_iter().next().unwrap().unwrap().2, U256::from(5));
        drop(db);
        drop(raw);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
