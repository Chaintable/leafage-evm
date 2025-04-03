use crate::db::{BlockIterator, BlockRead, StateDBIterator, StateDBWrite};
use crate::db_impl::{ArchiveRocksDBStorage, RocksDBStorage};
use std::path::Path;
use tracing::info;

pub struct DBource {
    src: ArchiveRocksDBStorage,
    dst: RocksDBStorage,
}

impl DBource {
    pub fn new<P: AsRef<Path>>(src_path: P, dst_path: P) -> Self {
        let src = ArchiveRocksDBStorage::open(src_path, 1024);
        let dst = RocksDBStorage::open(dst_path, 1024);
        Self { src, dst }
    }

    fn migrate_code(&self) -> anyhow::Result<()> {
        let mut batch = self.dst.prepare_write_batch()?;
        let mut iter = self.src.code_iter();
        let mut code_count = 0;
        while let Some(res) = iter.next() {
            let (code_hash, code) = res?;
            self.dst.write_code(&mut batch, code_hash, code)?;
            code_count += 1;
            if code_count % 10000 == 0 {
                self.dst.commit(batch)?;
                batch = self.dst.prepare_write_batch()?;
                info!(target = "migrate", "migrated code count: {}", code_count);
            }
        }
        self.dst.commit(batch)?;
        info!(
            target = "migrate",
            "migrated code done, count: {}", code_count
        );
        Ok(())
    }

    fn migrate_account(&self) -> anyhow::Result<()> {
        let mut batch = self.dst.prepare_write_batch()?;
        let mut iter = self.src.account_iter();
        let mut account_count = 0;
        while let Some(res) = iter.next() {
            let (address, account) = res?;
            self.dst
                .write_account(&mut batch, address, 0, Some(account))?;
            account_count += 1;
            if account_count % 200000 == 0 {
                self.dst.commit(batch)?;
                batch = self.dst.prepare_write_batch()?;
                info!(
                    target = "migrate",
                    "migrated account count: {}", account_count
                );
            }
        }
        self.dst.commit(batch)?;
        info!(
            target = "migrate",
            "migrated account done, count: {}", account_count
        );
        Ok(())
    }

    fn migrate_storage(&self) -> anyhow::Result<()> {
        let mut batch = self.dst.prepare_write_batch()?;
        let mut iter = self.src.storage_iter();
        let mut storage_count = 0;
        while let Some(res) = iter.next() {
            let (address, key, value) = res?;
            self.dst.write_storage(&mut batch, address, key, 0, value)?;
            storage_count += 1;
            if storage_count % 500000 == 0 {
                self.dst.commit(batch)?;
                batch = self.dst.prepare_write_batch()?;
                info!(
                    target = "migrate",
                    "migrated storage count: {}", storage_count
                )
            }
        }
        self.dst.commit(batch)?;
        info!(
            target = "migrate",
            "migrated storage done, count: {}", storage_count
        );
        Ok(())
    }

    fn migrate_block_info(&self) -> anyhow::Result<()> {
        let mut batch = self.dst.prepare_write_batch()?;
        let latest_block_hash = self.src.read_latest_block_hash()?;
        let latest_block_info = self.src.read_block_info(latest_block_hash)?;
        if latest_block_info.is_none() {
            return Err(anyhow::anyhow!("latest block info not found"));
        }
        let latest_block_info = latest_block_info.unwrap();
        self.dst
            .write_latest_block_hash(&mut batch, latest_block_hash)?;
        self.dst
            .write_block_info(&mut batch, latest_block_info.clone())?;
        let mut hash_iter = self.src.block_hash_iter();
        let mut hash_count = 0;
        while let Some(res) = hash_iter.next() {
            let (number, hash) = res?;
            self.dst.write_block_hash(&mut batch, number, hash)?;
            hash_count += 1;
            if hash_count % 1000000 == 0 {
                self.dst.commit(batch)?;
                batch = self.dst.prepare_write_batch()?;
                info!(
                    target = "migrate",
                    "migrated block hash count: {}", hash_count
                )
            }
        }
        self.dst.commit(batch)?;
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
