use crate::runner::run_until_ctrl_c;
use crate::updater::Updater;
use anyhow::{bail, Result};
use clap::Parser;
use leafage_evm_rpc::ApiBuilder;
use leafage_evm_storage::{RocksDBStorage, SnapshotTree, SnapshotTreeConfig, StateDBWrapper};
use revm::primitives::{CfgEnv, SpecId};
use serde_json::from_str;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

/// `leafage-evm standalone` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to Cfg config to use for this node.
    ///
    /// If not specified, the default config [eth] will be used.
    #[arg(long, value_name = "PATH")]
    chain_cfg_path: Option<PathBuf>,

    /// The path to the database to use for this node.
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// The type of database to use for this node.
    /// Default: rocksdb
    #[arg(long, default_value = "rocksdb")]
    db_type: String,

    /// The address for rpc client.
    #[arg(long, value_name = "URL")]
    rpc_addr: Option<String>,

    /// addr to listen on
    /// Default: 8545  
    ///
    /// This addr is used for the HTTP-RPC server
    #[arg(long, default_value = "0.0.0.0:8545")]
    listen_addr: String,

    /// The depth limit of the diff tree.
    /// Default: 64 for eth mainnet
    ///
    /// This limit is finalized block number - current block number.
    #[arg(long, default_value = "64")]
    diff_depth_limit: usize,

    /// The depth limit of the cache tree.
    /// Default: 512 for eth mainnet
    ///
    /// This limit is determined by the number of blocks that can be cached in memory.
    #[arg(long, default_value = "512")]
    cache_depth_limit: usize,

    /// The interval to fetch block and update the snapshot tree.
    /// Default: 5 seconds
    ///
    /// This interval is used to fetch block from rpc client.
    #[arg(long, value_parser = parse_duration, default_value = "5")]
    update_interval: std::time::Duration,
}

fn parse_duration(arg: &str) -> Result<std::time::Duration, std::num::ParseIntError> {
    let seconds = arg.parse()?;
    Ok(std::time::Duration::from_secs(seconds))
}

impl Command {
    async fn start(
        &self,
        chain_cfg: CfgEnv,
    ) -> Result<(
        tokio::sync::watch::Sender<()>,
        jsonrpsee::server::ServerHandle,
    )> {
        match self.db_type.as_str() {
            "rocksdb" => {
                let db = StateDBWrapper(RocksDBStorage::open(self.db_path.as_path()));
                let snaps = Arc::new(SnapshotTree::new(
                    db,
                    SnapshotTreeConfig::new(self.diff_depth_limit, self.cache_depth_limit),
                )?);
                let rpc_handle = ApiBuilder::new(snaps.clone(), chain_cfg.clone())
                    .build_and_run(&self.listen_addr)
                    .await?;
                if let Some(rpc_address) = self.rpc_addr.clone() {
                    let updater = Updater::new(snaps.clone(), rpc_address, self.update_interval)?;
                    let updater_handle = updater.start();
                    return Ok((updater_handle, rpc_handle));
                }
                Ok((tokio::sync::watch::channel(()).0, rpc_handle))
            }
            _ => bail!("only support rocksdb"),
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        let mut chain_cfg = CfgEnv::default();
        chain_cfg.spec_id = SpecId::SHANGHAI;
        if let Some(path) = self.chain_cfg_path.as_ref() {
            let data = fs::read_to_string(path.as_path())?;
            chain_cfg = from_str(&data)?;
        }
        let (updater_handle, rpc_handle) = self.start(chain_cfg).await?;
        run_until_ctrl_c(async move {
            info!("stopping leafage server...");
            let _ = updater_handle.send(());
            let _ = rpc_handle.stop();
            Ok(())
        })
        .await?;
        Ok(())
    }
}
