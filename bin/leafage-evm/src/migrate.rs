use anyhow::{bail, Result};
use clap::Parser;
use leafage_evm_storage::{FileSource, MigateStat, RocksDBStorage, StateDBWrite};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};
/// `leafage-evm migrate` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to the dir which contains the data to migrate
    ///
    /// shoud contains the following files:
    /// - lastest_block_info dir
    /// - storage dir
    /// - account dir
    /// - code dir
    #[arg(long, value_name = "PATH")]
    source_path: PathBuf,

    /// The path to the database to use for leafage-evm
    ///
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// The type of database to use for this node.
    /// Default: rocksdb
    /// current support rocksdb and leveldb
    #[arg(long, default_value = "rocksdb")]
    db_type: String,

    /// The number of code count to skip
    /// Default: 0
    ///
    /// use to resume from break-point
    #[arg(long, default_value = "0")]
    skip_code_count: u64,

    /// The number of account count to skip
    /// Default: 0
    ///
    /// use to resume from break-point
    #[arg(long, default_value = "0")]
    skip_account_count: u64,

    /// The number of storage count to skip
    /// Default: 0
    ///
    /// use to resume from break-point
    #[arg(long, default_value = "0")]
    skip_storage_count: u64,
}

async fn db_migration<DB: StateDBWrite>(
    block_info_source: FileSource,
    storage_source: FileSource,
    account_source: FileSource,
    code_source: FileSource,
    db: Arc<DB>,
) -> Result<()> {
    let db1 = db.clone();
    let join1 = tokio::spawn(async move {
        let res = block_info_source.migrate_block_info(db1.clone()).await;
        if res.is_err() {
            error!(target:"migrate","migrate_block_info failed, {:?}",res);
        }
        res
    });
    let db2 = db.clone();
    let join2 = tokio::spawn(async move {
        let res = storage_source.migrate_storage(db2.clone()).await;
        if res.is_err() {
            error!(target:"migrate","migrate_storage failed, {:?}",res);
        }
        res
    });
    let db3 = db.clone();
    let join3 = tokio::spawn(async move {
        let res = account_source.migrate_account(db3.clone()).await;
        if res.is_err() {
            error!(target:"migrate","migrate_account failed, {:?}",res);
        }
        res
    });
    let db4 = db.clone();
    let join4 = tokio::spawn(async move {
        let res = code_source.migrate_code(db4.clone()).await;
        if res.is_err() {
            error!(target:"migrate","migrate_code failed, {:?}",res);
        }
        res
    });
    let res = tokio::join!(join1, join2, join3, join4);
    let _ = res.0?;
    let _ = res.1?;
    let _ = res.2?;
    let _ = res.3?;
    Ok(())
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        let block_info_path = self.source_path.join("lastest_block_info");
        let storage_path = self.source_path.join("storages");
        let account_path = self.source_path.join("accounts");
        let code_path = self.source_path.join("codes");
        info!(target:"migrate","block_info_path: {:?}",block_info_path);
        let stat = Arc::new(MigateStat::default());
        let block_info_source = FileSource::new(
            block_info_path,
            |entry| entry.file_name().to_str().unwrap().ends_with(".json"),
            stat.clone(),
        )?;
        let storage_source = FileSource::new(
            storage_path,
            |entry| {
                let name = entry
                    .file_name()
                    .to_ascii_lowercase()
                    .to_str()
                    .unwrap()
                    .to_string();
                name.ends_with(".rlp") && name > format!("{}.rlp", self.skip_storage_count)
            },
            stat.clone(),
        )?;
        let account_source = FileSource::new(
            account_path,
            |entry| {
                let name = entry
                    .file_name()
                    .to_ascii_lowercase()
                    .to_str()
                    .unwrap()
                    .to_string();
                name.ends_with(".rlp") && name > format!("{}.rlp", self.skip_account_count)
            },
            stat.clone(),
        )?;
        let code_source = FileSource::new(
            code_path,
            |entry| {
                let name = entry
                    .file_name()
                    .to_ascii_lowercase()
                    .to_str()
                    .unwrap()
                    .to_string();
                name.ends_with(".rlp") && name > format!("{}.rlp", self.skip_code_count)
            },
            stat.clone(),
        )?;
        let (tx, rx) = tokio::sync::watch::channel(());
        let stat_join = tokio::spawn(async move {
            let stat = stat;
            let mut rx = rx;
            let mut time = tokio::time::interval(std::time::Duration::from_secs(4));
            loop {
                tokio::select! {
                    _ = time.tick() => {
                        let code_count = stat.code_count.load(std::sync::atomic::Ordering::SeqCst);
                        let account_count = stat.account_count.load(std::sync::atomic::Ordering::SeqCst);
                        let storage_count = stat.storage_count.load(std::sync::atomic::Ordering::SeqCst);
                        info!(target:"migrate","code_count: {}, account_count: {}, storage_count: {}",code_count,account_count,storage_count);
                    }

                    _ = rx.changed() => {
                        let code_count = stat.code_count.load(std::sync::atomic::Ordering::SeqCst);
                        let account_count = stat.account_count.load(std::sync::atomic::Ordering::SeqCst);
                        let storage_count = stat.storage_count.load(std::sync::atomic::Ordering::SeqCst);
                        info!(target:"migrate","migrate done, code_count: {}, account_count: {}, storage_count: {}",code_count,account_count,storage_count);
                        break;
                    }
                }
            }
        });
        match self.db_type.as_str() {
            "rocksdb" => {
                let db = Arc::new(RocksDBStorage::open(self.db_path.clone()));
                db_migration(
                    block_info_source,
                    storage_source,
                    account_source,
                    code_source,
                    db,
                )
                .await?;
            }
            _ => bail!("Unsupported db type"),
        }
        tx.send(()).unwrap();
        stat_join.await?;
        Ok(())
    }
}
