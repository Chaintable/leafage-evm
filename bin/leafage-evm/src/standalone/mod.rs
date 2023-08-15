use crate::runner::run_until_ctrl_c;
use crate::updater::Updater;
use anyhow::{bail, Result};
use clap::Parser;
use leafage_evm_rpc::ApiBuilder;
use leafage_evm_storage::{RocksDBStorage, SnapshotTree};
use revm::primitives::CfgEnv;
use serde_json::from_str;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

/// `leafage-evm standalone` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to Cfg config to use for this node.
    ///
    /// If not specified, the default config will be used.
    #[arg(long, value_name = "PATH")]
    chain_cfg_path: Option<PathBuf>,

    /// The path to the database to use for this node.
    ///
    /// current support rocksdb and memorydb
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// The type of database to use for this node.
    /// Default: rocksdb
    /// current support rocksdb and memorydb
    #[arg(long, default_value = "rocksdb")]
    db_type: String,

    /// The address for rpc client.
    #[arg(long, value_name = "URL")]
    rpc_addr: String,

    /// addr to listen on
    /// Default: 8545  
    ///
    /// This addr is used for the HTTP-RPC server
    #[arg(long, default_value = "0.0.0.0:8545")]
    listen_addr: String,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        let mut chain_cfg = CfgEnv::default();
        if let Some(path) = self.chain_cfg_path.as_ref() {
            let data = fs::read_to_string(path.as_path())?;
            chain_cfg = from_str(&data)?;
        }
        if self.db_type != "rocksdb" {
            bail!("only support rocksdb")
        }
        let db = RocksDBStorage::open(self.db_path.as_path());
        let snaps = Arc::new(SnapshotTree::new(db)?);
        let updater = Updater::new(snaps.clone(), self.rpc_addr.clone())?;
        let updater_handle = updater.start();
        let rpc_handle = ApiBuilder::new(snaps.clone(), chain_cfg.clone())
            .build_and_run(&self.listen_addr)
            .await?;
        run_until_ctrl_c(async move {
            let _ = updater_handle.send(());
            let _ = rpc_handle.stop();
            Ok(())
        })
        .await?;
        Ok(())
    }
}
