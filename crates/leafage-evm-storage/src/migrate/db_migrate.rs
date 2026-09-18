use crate::db::{BlockIterator, LatestStateDBIterator, StateDBProvider, StateDBWrite};
use crate::db_impl::BulkColumn;
use crate::db_impl::{
    state_account_value, state_block_num_key, state_sst_writer_options, state_storage_key,
    MultiStorage, RocksDBStorage, SstSink, StorageError, StorageKind,
};
use crate::BlockContext;
use leafage_evm_types::{BlockId, BlockNumberOrTag};
use std::path::Path;
use tracing::info;

pub struct DBSource {
    src: MultiStorage,
    dst: MultiStorage,
}

impl DBSource {
    pub fn new<P: AsRef<Path>>(
        src_path: P,
        src_kind: StorageKind,
        src_is_archive: bool,
        dst_path: P,
        dst_kind: StorageKind,
        cache_size: usize,
    ) -> Result<Self, StorageError> {
        let src = MultiStorage::open(src_path, cache_size, src_kind, src_is_archive, false, false)?;
        let dst = MultiStorage::open(dst_path, cache_size, dst_kind, false, false, false)?;

        // 验证 dst 不是 archive 类型
        if matches!(
            dst,
            MultiStorage::RocksDBArchive(_) | MultiStorage::MDBXArchive(_)
        ) {
            return Err(StorageError::UnSupported(
                "Destination storage cannot be archive type".to_string(),
            ));
        }

        Ok(Self { src, dst })
    }

    fn migrate_code(&self) -> anyhow::Result<()> {
        let dst_db = self
            .dst
            .db_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or(anyhow::anyhow!("failed to get destination state db"))?;
        let mut batch = dst_db.prepare_write_batch()?;
        let mut iter = self.src.code_iter();
        let mut code_count = 0;
        while let Some(res) = iter.next() {
            let (code_hash, code) = res?;
            dst_db.write_code(&mut batch, code_hash, code)?;
            code_count += 1;
            if code_count % 10000 == 0 {
                dst_db.commit(batch)?;
                batch = dst_db.prepare_write_batch()?;
                info!(target = "migrate", "migrated code count: {}", code_count);
            }
        }
        dst_db.commit(batch)?;
        info!(
            target = "migrate",
            "migrated code done, count: {}", code_count
        );
        Ok(())
    }

    fn migrate_account(&self) -> anyhow::Result<()> {
        let dst_db = self
            .dst
            .db_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or(anyhow::anyhow!("failed to get destination state db"))?;
        let mut batch = dst_db.prepare_write_batch()?;
        let mut iter = self.src.account_iter();
        let mut account_count = 0;
        while let Some(res) = iter.next() {
            let (address, account) = res?;
            dst_db.write_account(&mut batch, address, 0, Some(account))?;
            account_count += 1;
            if account_count % 200000 == 0 {
                dst_db.commit(batch)?;
                batch = dst_db.prepare_write_batch()?;
                info!(
                    target = "migrate",
                    "migrated account count: {}", account_count
                );
            }
        }
        dst_db.commit(batch)?;
        info!(
            target = "migrate",
            "migrated account done, count: {}", account_count
        );
        Ok(())
    }

    fn migrate_storage(&self) -> anyhow::Result<()> {
        let dst_db = self
            .dst
            .db_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or(anyhow::anyhow!("failed to get destination state db"))?;
        let mut batch = dst_db.prepare_write_batch()?;
        let mut iter = self.src.storage_iter();
        let mut storage_count = 0;
        while let Some(res) = iter.next() {
            let (address, key, value) = res?;
            dst_db.write_storage(&mut batch, address, key, 0, value)?;
            storage_count += 1;
            if storage_count % 500000 == 0 {
                dst_db.commit(batch)?;
                batch = dst_db.prepare_write_batch()?;
                info!(
                    target = "migrate",
                    "migrated storage count: {}", storage_count
                )
            }
        }
        dst_db.commit(batch)?;
        info!(
            target = "migrate",
            "migrated storage done, count: {}", storage_count
        );
        Ok(())
    }

    /// The head pointer and the block info behind it: two records, written
    /// through the batch path by both migration paths.
    fn migrate_latest_block(&self) -> anyhow::Result<()> {
        let dst_db = self
            .dst
            .db_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or(anyhow::anyhow!("failed to get destination state db"))?;
        let mut batch = dst_db.prepare_write_batch()?;
        let src_statedb = self
            .src
            .state_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or(anyhow::anyhow!("failed to get source latest state db"))?;
        let latest_block_info = src_statedb.block_info()?;
        dst_db.write_latest_block_hash(&mut batch, latest_block_info.hash())?;
        dst_db.write_block_info(&mut batch, latest_block_info)?;
        dst_db.commit(batch)?;
        Ok(())
    }

    fn migrate_block_info(&self) -> anyhow::Result<()> {
        self.migrate_latest_block()?;
        let dst_db = self
            .dst
            .db_at(BlockId::Number(BlockNumberOrTag::Latest))?
            .ok_or(anyhow::anyhow!("failed to get destination state db"))?;
        let mut batch = dst_db.prepare_write_batch()?;
        let mut hash_iter = self.src.block_hash_iter();
        let mut hash_count = 0;
        while let Some(res) = hash_iter.next() {
            let (number, hash) = res?;
            dst_db.write_block_hash(&mut batch, number, hash)?;
            hash_count += 1;
            if hash_count % 1000000 == 0 {
                dst_db.commit(batch)?;
                batch = dst_db.prepare_write_batch()?;
                info!(
                    target = "migrate",
                    "migrated block hash count: {}", hash_count
                )
            }
        }
        dst_db.commit(batch)?;
        info!(
            target = "migrate",
            "migrated block hash done, count: {}", hash_count
        );
        Ok(())
    }

    /// Records between progress lines. The batch path logs per commit; the
    /// bulk path has no commits to hang them on, so it counts.
    const PROGRESS_EVERY: u64 = 1_000_000;

    /// SST staging files are rolled at this size. Any split of an ascending
    /// stream yields non-overlapping files, which ingest can place across
    /// levels instead of piling all of them into L0.
    const SST_ROLL_BYTES: u64 = 512 * 1024 * 1024;

    /// Migration into a RocksDB state DB, writing SST files and handing them to
    /// `ingest_external_file` instead of committing write batches.
    ///
    /// Every column family a migration fills is written exactly once, in
    /// ascending key order — which is the contract an SST file has to meet, and
    /// what a scan of the source hands over for free. The records then skip the
    /// WAL, the memtable, and the L0→Ln compactions a batch write pays for
    /// afterwards; on a mainnet-sized archive that is most of the destination's
    /// write I/O.
    ///
    /// The scans drop accounts deleted at the tip and slots that are zero
    /// there. The batch path turns those into `delete_cf` calls, which is the
    /// same thing on a destination that never held the key.
    fn migrate_bulk(&self, dst: &RocksDBStorage) -> anyhow::Result<()> {
        // Staged inside the destination so ingest moves the files in rather
        // than copying them across filesystems.
        let staging = dst.path().join(".migrate_sst");
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let opts = state_sst_writer_options();

        let mut count = 0u64;
        let mut sink = SstSink::new(
            &opts,
            staging.clone(),
            "account".to_string(),
            Self::SST_ROLL_BYTES,
        );
        for item in self.src.account_iter() {
            let (address, account) = item?;
            sink.put(address.as_slice(), &state_account_value(account))?;
            count += 1;
            if count % Self::PROGRESS_EVERY == 0 {
                info!(target = "migrate", "migrated account count: {}", count);
            }
        }
        dst.ingest_bulk(BulkColumn::Account, sink.finish()?)?;
        info!(
            target = "migrate",
            "migrated account done, count: {}", count
        );

        count = 0;
        let mut sink = SstSink::new(
            &opts,
            staging.clone(),
            "code".to_string(),
            Self::SST_ROLL_BYTES,
        );
        for item in self.src.code_iter() {
            let (code_hash, code) = item?;
            sink.put(code_hash.as_slice(), code.as_ref())?;
            count += 1;
        }
        dst.ingest_bulk(BulkColumn::Code, sink.finish()?)?;
        info!(target = "migrate", "migrated code done, count: {}", count);

        count = 0;
        let mut sink = SstSink::new(
            &opts,
            staging.clone(),
            "storage".to_string(),
            Self::SST_ROLL_BYTES,
        );
        for item in self.src.storage_iter() {
            let (address, index, value) = item?;
            let value_bytes: [u8; 32] = value.to_be_bytes();
            sink.put(&state_storage_key(address, index), &value_bytes)?;
            count += 1;
            if count % Self::PROGRESS_EVERY == 0 {
                info!(target = "migrate", "migrated storage count: {}", count);
            }
        }
        dst.ingest_bulk(BulkColumn::Storage, sink.finish()?)?;
        info!(
            target = "migrate",
            "migrated storage done, count: {}", count
        );

        count = 0;
        let mut sink = SstSink::new(
            &opts,
            staging.clone(),
            "block_hash".to_string(),
            Self::SST_ROLL_BYTES,
        );
        for item in self.src.block_hash_iter() {
            let (number, hash) = item?;
            sink.put(&state_block_num_key(number), hash.as_slice())?;
            count += 1;
        }
        dst.ingest_bulk(BulkColumn::BlockHash, sink.finish()?)?;
        info!(
            target = "migrate",
            "migrated block hash done, count: {}", count
        );

        self.migrate_latest_block()?;
        std::fs::remove_dir_all(&staging)?;
        Ok(())
    }

    pub fn migrate(&self) -> anyhow::Result<()> {
        info!(target = "migrate", "migrating all data...");
        match &self.dst {
            MultiStorage::RocksDBState(dst) => self.migrate_bulk(dst)?,
            // MDBX has no SstFileWriter; it keeps the batch path.
            _ => {
                self.migrate_account()?;
                self.migrate_code()?;
                self.migrate_storage()?;
                self.migrate_block_info()?;
            }
        }
        info!(target = "migrate", "migrated all data done");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{StateDBRead, StateDBWrapper};
    use crate::EvmStorageWrite;
    use leafage_evm_types::{
        AccountStorageDiff, Block, BlockInfo, BlockStorageDiff, Bytes, Header, IndexValuePair,
        NewAccount, NewCode, RawHeader, H256, KECCAK256_EMPTY, U256,
    };

    fn block_info(number: u64) -> BlockInfo {
        let mut raw = RawHeader::default();
        raw.number = number;
        BlockInfo::new(Block {
            header: Header {
                hash: H256::with_last_byte(number as u8),
                inner: raw,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn account(address: H256, balance: u64, code_hash: H256) -> NewAccount {
        NewAccount {
            address,
            balance: U256::from(balance),
            nonce: 1,
            code_hash,
        }
    }

    /// An archive holding two versions of everything, migrated into a state DB:
    /// the destination must come out holding the tip and nothing else. Covers
    /// what the bulk path builds SST files from — accounts, code, storage,
    /// block hashes — plus the head pointer the batch path still writes.
    #[test]
    fn migration_writes_the_tip_state_of_every_column_family() {
        let _guard = crate::db_impl::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let base = std::env::temp_dir().join(format!("leafage-db-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src_path = base.join("archive");
        let dst_path = base.join("state");

        let kept = H256::repeat_byte(0x11);
        let deleted = H256::repeat_byte(0x22);
        let slot = H256::repeat_byte(0x01);
        let emptied = H256::repeat_byte(0x02);
        let code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]);
        let code_hash = H256::repeat_byte(0xcd);

        {
            let src = MultiStorage::open(&src_path, 64, StorageKind::Rocksdb, true, false, false)
                .unwrap();
            let write = |number: u64, diff: BlockStorageDiff| {
                let state = StateDBWrapper(
                    src.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                        .unwrap()
                        .unwrap(),
                );
                state.update_block(block_info(number), diff).unwrap();
            };

            write(
                1,
                BlockStorageDiff {
                    new_accounts: vec![
                        account(kept, 10, KECCAK256_EMPTY.0.into()),
                        account(deleted, 20, KECCAK256_EMPTY.0.into()),
                    ],
                    new_codes: vec![NewCode {
                        code_hash,
                        code: code.clone(),
                    }],
                    storage_diffs: vec![AccountStorageDiff {
                        address: kept,
                        diffs: vec![
                            IndexValuePair {
                                index: slot,
                                value: U256::from(1),
                            },
                            IndexValuePair {
                                index: emptied,
                                value: U256::from(2),
                            },
                        ],
                    }],
                    ..Default::default()
                },
            );
            // Second version of each: the migration must take these, not the
            // records from block 1.
            write(
                2,
                BlockStorageDiff {
                    new_accounts: vec![account(kept, 11, code_hash)],
                    deleted_accounts: vec![deleted],
                    storage_diffs: vec![AccountStorageDiff {
                        address: kept,
                        diffs: vec![
                            IndexValuePair {
                                index: slot,
                                value: U256::from(5),
                            },
                            IndexValuePair {
                                index: emptied,
                                value: U256::ZERO,
                            },
                        ],
                    }],
                    ..Default::default()
                },
            );
        }

        DBSource::new(
            &src_path,
            StorageKind::Rocksdb,
            true,
            &dst_path,
            StorageKind::Rocksdb,
            64,
        )
        .unwrap()
        .migrate()
        .unwrap();

        let dst =
            MultiStorage::open(&dst_path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
        let state = dst
            .db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap();

        let migrated = state.read_account(kept).unwrap().unwrap();
        assert_eq!(migrated.balance, U256::from(11));
        assert_eq!(migrated.code_hash, code_hash);
        assert!(
            state.read_account(deleted).unwrap().is_none(),
            "account deleted at the tip must not be migrated"
        );
        assert_eq!(state.read_storage(kept, slot).unwrap(), U256::from(5));
        assert_eq!(
            state.read_storage(kept, emptied).unwrap(),
            U256::ZERO,
            "slot zeroed at the tip must not be migrated"
        );
        assert_eq!(state.read_code(code_hash).unwrap().unwrap(), code);
        assert_eq!(state.read_block_hash(2).unwrap(), block_info(2).hash());
        let head = state.read_latest_block_hash().unwrap();
        assert_eq!(head, block_info(2).hash());
        assert_eq!(
            state.read_block_info(head).unwrap().unwrap().header.number,
            2
        );

        // The staging directory is cleaned up, so the destination is only the DB.
        assert!(!dst_path.join(".migrate_sst").exists());

        drop(dst);
        let _ = std::fs::remove_dir_all(&base);
    }
}
