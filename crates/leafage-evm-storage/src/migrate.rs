use crate::db::StateDBWrite;
use alloy_rlp::Decodable;
use alloy_rlp_derive::RlpDecodable;
use leafage_evm_types::{Block, Bytes, NewAccount, Transaction, H256, U256};
use std::fs::DirEntry;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

async fn read_file_to_code(entry: DirEntry) -> Result<Vec<(H256, Bytes)>, std::io::Error> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(entry.path()).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    bytes_to_codes(&buf)
}

#[derive(RlpDecodable)]
struct CodeWithHash {
    code_hash: H256,
    code: Bytes,
}

fn bytes_to_codes(mut buf: &[u8]) -> Result<Vec<(H256, Bytes)>, std::io::Error> {
    let codes: Vec<CodeWithHash> = Decodable::decode(&mut buf).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to decode codes")
    })?;
    let codes = codes
        .into_iter()
        .map(|code| (code.code_hash, code.code))
        .collect();
    Ok(codes)
}

async fn read_file_to_account(entry: DirEntry) -> Result<Vec<NewAccount>, std::io::Error> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(entry.path()).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    bytes_to_accounts(&buf)
}

fn bytes_to_accounts(mut buf: &[u8]) -> Result<Vec<NewAccount>, std::io::Error> {
    let accounts: Vec<NewAccount> = Decodable::decode(&mut buf).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to decode accounts")
    })?;
    Ok(accounts)
}

async fn read_file_to_storage(entry: DirEntry) -> Result<Vec<(H256, H256, U256)>, std::io::Error> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(entry.path()).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    bytes_to_storages(&buf)
}

#[derive(RlpDecodable)]
struct KeyValue {
    address: H256,
    index: H256,
    val: U256,
}

fn bytes_to_storages(mut buf: &[u8]) -> Result<Vec<(H256, H256, U256)>, std::io::Error> {
    let storages: Vec<KeyValue> = Decodable::decode(&mut buf).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "Failed to decode storages")
    })?;
    let storages = storages
        .into_iter()
        .map(|kv| (kv.address, kv.index, kv.val))
        .collect();
    Ok(storages)
}

async fn read_file_to_block_info(entry: DirEntry) -> Result<Block<Transaction>, std::io::Error> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(entry.path()).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    bytes_to_block_info(&buf)
}

fn bytes_to_block_info(buf: &[u8]) -> Result<Block<Transaction>, std::io::Error> {
    let block_info = serde_json::from_slice(buf).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Failed to decode block info",
        )
    })?;
    Ok(block_info)
}

/// [`MigateStat`] is used to record the number of migrated data.
#[derive(Debug, Default)]
pub struct MigateStat {
    pub code_count: AtomicU64,
    pub account_count: AtomicU64,
    pub storage_count: AtomicU64,
}

/// [`FileSource`] is used to read data from files.
pub struct FileSource {
    files: Vec<DirEntry>,
    stat: Arc<MigateStat>,
}

impl FileSource {
    pub fn new<F: Fn(&DirEntry) -> bool>(
        path: PathBuf,
        filter: F,
        stat: Arc<MigateStat>,
    ) -> Result<Self, std::io::Error> {
        let mut files = Vec::new();
        let mut entries = std::fs::read_dir(path)?;
        while let Some(entry) = entries.next() {
            let entry = entry?;
            if filter(&entry) {
                files.push(entry);
            }
        }
        if files.len() == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No files found",
            ));
        }
        files.sort_by_key(|entry| entry.file_name().to_ascii_lowercase());
        Ok(Self { files, stat })
    }

    /// Migrate code from files to db.
    pub async fn migrate_code<DB: StateDBWrite>(self, db: DB) -> anyhow::Result<()> {
        for file in self.files.into_iter() {
            let mut batch = db.prepare_write_batch()?;
            let codes = read_file_to_code(file).await?;
            for (code_hash, code) in codes {
                db.write_code(&mut batch, code_hash, code)?;
                self.stat.code_count.fetch_add(1, Ordering::SeqCst);
            }
            db.commit(batch)?;
        }
        Ok(())
    }

    /// Migrate account from files to db.
    pub async fn migrate_account<DB: StateDBWrite>(self, db: DB) -> anyhow::Result<()> {
        for file in self.files.into_iter() {
            let mut batch = db.prepare_write_batch()?;
            let accounts = read_file_to_account(file).await?;
            for account in accounts {
                db.write_account(&mut batch, account.address, Some(account))?;
                self.stat.account_count.fetch_add(1, Ordering::SeqCst);
            }
            db.commit(batch)?;
        }
        Ok(())
    }

    /// Migrate storage from files to db.
    pub async fn migrate_storage<DB: StateDBWrite>(self, db: DB) -> anyhow::Result<()> {
        for file in self.files.into_iter() {
            let mut batch = db.prepare_write_batch()?;
            let storages = read_file_to_storage(file).await?;
            for (address, key, value) in storages {
                db.write_storage(&mut batch, address, key, value)?;
                self.stat.storage_count.fetch_add(1, Ordering::SeqCst);
            }
            db.commit(batch)?;
        }
        Ok(())
    }

    /// Migrate block info from files to db.
    pub async fn migrate_block_info<DB: StateDBWrite>(self, db: DB) -> anyhow::Result<()> {
        for file in self.files.into_iter() {
            let mut batch = db.prepare_write_batch()?;
            let block_info = read_file_to_block_info(file).await?;
            db.write_latest_block_hash(&mut batch, block_info.header.hash.unwrap())?;
            db.write_block_hash(
                &mut batch,
                block_info.header.number.unwrap(),
                block_info.header.hash.unwrap(),
            )?;
            db.write_block_info(&mut batch, block_info)?;
            db.commit(batch)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_account_read() {
        use std::io::Read;
        let mut file = std::fs::File::open("/data/nodex/accounts/99900000.rlp").unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        let accounts = bytes_to_accounts(&buf).unwrap();
        dbg!(&accounts[1]);
    }
}
