use super::*;
use crate::db_impl::rewind::{
    remove_record, RewindTable, REWIND_BATCH_BYTES, REWIND_BATCH_SIZE, REWIND_TABLES,
};

impl DataBaseRef {
    pub(crate) fn rewind_layout(&self) -> Result<(Option<Vec<u8>>, bool), Error> {
        let marker = self
            .db
            .get_cf(self.db.cf_handle("1").unwrap(), ENCODING_MARKER_KEY)?;
        let mut populated = false;
        for (cf, table) in [("4", RewindTable::Accounts), ("5", RewindTable::Storage)] {
            let mut opts = ReadOptions::default();
            opts.set_total_order_seek(true);
            opts.set_verify_checksums(true);
            if let Some(item) = self
                .db
                .iterator_cf_opt(self.db.cf_handle(cf).unwrap(), opts, IteratorMode::Start)
                .next()
            {
                let (key, value) = item?;
                remove_record(table, &key, &value, 0, false, false)?;
                populated = true;
            }
        }
        Ok((marker, populated))
    }

    pub(crate) fn rewind_to(&self, target: &BlockInfo, inverted: bool) -> Result<(), Error> {
        let meta = self
            .db
            .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
            .unwrap();
        let mut options = WriteOptions::default();
        options.set_sync(true);

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
            read_options.set_verify_checksums(true);
            read_options.fill_cache(false);
            let mut batch = WriteBatch::default();
            let mut deleted = 0u64;
            for item in self
                .db
                .iterator_cf_opt(cf, read_options, IteratorMode::Start)
            {
                let (key, value) = item?;
                if remove_record(table, &key, &value, target.header.number, inverted, false)? {
                    batch.delete_cf(cf, key);
                    deleted += 1;
                    if batch.len() >= REWIND_BATCH_SIZE
                        || batch.size_in_bytes() >= REWIND_BATCH_BYTES
                    {
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

        assert!(db
            .rewind_archive(
                0,
                Some(inverted_block_encoding()),
                &dir.join("offset/offset")
            )
            .is_err());
        assert!(db
            .rewind_archive(
                5,
                Some(inverted_block_encoding()),
                &dir.join("offset/offset")
            )
            .is_err());
        db.rewind_archive(
            2,
            Some(inverted_block_encoding()),
            &dir.join("offset/offset"),
        )
        .unwrap();
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
        db.rewind_archive(
            2,
            Some(inverted_block_encoding()),
            &dir.join("offset/offset"),
        )
        .unwrap();
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
    fn malformed_storage_stops_truncation_without_publishing_target() {
        let _lock = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_inverted_block_encoding(false);
        let dir =
            std::env::temp_dir().join(format!("archive-rewind-malformed-{}", std::process::id()));
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
        raw.db
            .put_cf(
                raw.db.cf_handle("5").unwrap(),
                encode_storage_key(H256::repeat_byte(10), H256::repeat_byte(1), 1),
                U256::from(5).to_be_bytes::<32>(),
            )
            .unwrap();
        // A corrupt storage key fails after account truncation has committed.
        // The command must stop and leave the committed head unchanged.
        raw.db
            .put_cf(raw.db.cf_handle("5").unwrap(), [255u8], [0u8])
            .unwrap();
        assert!(db
            .rewind_archive(
                1,
                Some(inverted_block_encoding()),
                &dir.join("offset/offset")
            )
            .is_err());
        assert_eq!(raw.read_latest_block_hash().unwrap(), H256::repeat_byte(2));
        assert!(raw
            .db
            .get_cf(
                raw.db.cf_handle("4").unwrap(),
                encode_account_key(H256::repeat_byte(10), 2)
            )
            .unwrap()
            .is_none());
        drop(db);
        drop(raw);
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
        db.rewind_archive(
            1,
            Some(inverted_block_encoding()),
            &dir.join("offset/offset"),
        )
        .unwrap();
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
    #[test]
    fn prechecks_and_offset_failure_leave_state_unchanged() {
        let _lock = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_inverted_block_encoding(false);
        let dir =
            std::env::temp_dir().join(format!("archive-rewind-precheck-{}", std::process::id()));
        let raw = Arc::new(DataBaseRef::open(&dir, 16, false, false));
        let db = MultiStorage::RocksDBArchive(raw.clone());
        for n in 1..=2 {
            commit(
                &db,
                block(n, n as u8, n as u8 - 1),
                BlockStorageDiff {
                    new_accounts: vec![account(10, n)],
                    ..Default::default()
                },
            );
        }
        let meta = raw.db.cf_handle("1").unwrap();
        let file = dir.join("offset/offset");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "456").unwrap();
        assert!(db.rewind_archive(1, None, &file).is_err());
        for value in [vec![], vec![2], vec![0, 1]] {
            raw.db.put_cf(meta, ENCODING_MARKER_KEY, value).unwrap();
            assert!(db.rewind_archive(1, Some(false), &file).is_err());
        }
        raw.db.put_cf(meta, ENCODING_MARKER_KEY, [1]).unwrap();
        assert!(db.rewind_archive(1, Some(false), &file).is_err());
        raw.db.delete_cf(meta, ENCODING_MARKER_KEY).unwrap();
        // Snapshot-shaped keys must be rejected before state/offset mutation.
        raw.db
            .put_cf(raw.db.cf_handle("5").unwrap(), [0u8; 64], [0u8; 32])
            .unwrap();
        assert!(db.rewind_archive(1, Some(false), &file).is_err());
        raw.db
            .delete_cf(raw.db.cf_handle("5").unwrap(), [0u8; 64])
            .unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "456");
        // Force unlink failure. No archive records may be deleted.
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(db.rewind_archive(1, Some(false), &file).is_err());
        assert_eq!(raw.read_latest_block_hash().unwrap(), H256::repeat_byte(2));
        assert!(raw
            .db
            .get_cf(
                raw.db.cf_handle("4").unwrap(),
                encode_account_key(H256::repeat_byte(10), 2)
            )
            .unwrap()
            .is_some());
        std::fs::remove_dir(&file).unwrap();
        // A node without an existing offset file still truncates successfully.
        db.rewind_archive(1, Some(false), &file).unwrap();
        assert_eq!(raw.read_latest_block_hash().unwrap(), H256::repeat_byte(1));
        drop(db);
        drop(raw);
        unsafe {
            super::super::DATA_BASE = None;
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn header_deletion_uses_normal_reader_fallback() {
        let header = RawHeader {
            number: 42,
            ..Default::default()
        };
        let mut bytes = Vec::new();
        alloy_rlp::Encodable::encode(&header, &mut bytes);
        // An extra chain field makes strict RawHeader decoding fail, while the
        // existing reader accepts the classic prefix. Preserve that behavior.
        let mut payload = bytes.as_slice();
        let mut list = alloy_rlp::Header::decode(&mut payload).unwrap();
        list.payload_length += 1;
        let mut extended = Vec::new();
        list.encode(&mut extended);
        extended.extend_from_slice(payload);
        extended.push(0xc0);
        assert!(RawHeader::decode(&mut extended.as_slice()).is_err());
        assert_eq!(
            decode_archive_header(&mut extended.as_slice())
                .unwrap()
                .number,
            42
        );
        assert!(
            remove_record(RewindTable::Headers, &[1; 32], &extended, 41, false, false).unwrap()
        );
        assert!(
            !remove_record(RewindTable::Headers, &[1; 32], &extended, 42, false, false).unwrap()
        );
    }
}
