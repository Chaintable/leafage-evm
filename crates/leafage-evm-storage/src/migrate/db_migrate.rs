use crate::db::{BlockIterator, LatestStateDBIterator, StateDBProvider, StateDBWrite};
use crate::db_impl::{MultiStorage, StorageError, StorageKind};
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
        let src = MultiStorage::open(
            src_path,
            cache_size,
            src_kind,
            src_is_archive,
            false,
            false,
        )?;
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

    fn migrate_block_info(&self) -> anyhow::Result<()> {
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
        dst_db.write_block_info(&mut batch, latest_block_info.clone())?;
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

    pub fn migrate(&self) -> anyhow::Result<()> {
        info!(target = "migrate", "migrating all data...");
        self.migrate_account()?;
        self.migrate_code()?;
        self.migrate_storage()?;
        self.migrate_block_info()?;
        info!(target = "migrate", "migrated all data done");
        Ok(())
    }
}
