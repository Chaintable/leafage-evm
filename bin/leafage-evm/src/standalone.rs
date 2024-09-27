use crate::metrics;
use crate::runner::run_until_ctrl_c;
use crate::updater::Updater;
use anyhow::{bail, Result};
use clap::Parser;
use leafage_evm_rpc::ApiBuilder;
use leafage_evm_storage::{RocksDBStorage, SnapshotTree, SnapshotTreeConfig, StateDBWrapper};
use revm::primitives::CfgEnv;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

/// `leafage-evm standalone` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to Cfg config to use for this node.
    ///
    /// If not specified, the default config [eth] will be used.
    #[arg(long, value_parser = parse_chain_cfg, default_value = "eth")]
    chain_cfg: CfgEnv,

    /// The path to the database to use for this node.
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// The type of database to use for this node.
    /// Default: rocksdb
    #[arg(long, default_value = "rocksdb")]
    db_type: String,

    /// The size of the database cache in MB.
    /// Default: 1024
    ///
    /// This limit is used for the rocksdb cache.
    #[arg(long, default_value = "1024")]
    db_cache: usize,

    /// The address for rpc client.
    #[arg(long, value_name = "URL")]
    rpc_addr: Option<String>,

    /// addr to listen on
    /// Default: 8545  
    ///
    /// This addr is used for the HTTP-RPC server
    #[arg(long, default_value = "0.0.0.0:8545")]
    listen_addr: String,

    /// The maximum number of concurrent connections.
    /// Default: 5000
    ///
    /// This limit is used for the HTTP-RPC server
    #[arg(long, default_value = "5000")]
    max_connections: u32,

    /// The depth limit of the diff tree.
    /// Default: 64 for eth mainnet
    ///
    /// This limit is finalized block number - current block number.
    #[arg(long, default_value = "64")]
    diff_depth_limit: usize,

    /// The size of the account cache.
    /// Default: 200000
    ///
    /// This limit is used for the account cache.
    #[arg(long, default_value = "200000")]
    account_cache_size: usize,

    /// The size of the storage cache.
    /// Default: 5000000
    ///
    /// This limit is used for the storage cache.
    #[arg(long, default_value = "5000000")]
    storage_cache_size: usize,

    /// The size of the code cache.
    /// Default: 50000
    ///
    /// This limit is used for the code cache.
    #[arg(long, default_value = "50000")]
    code_cache_size: usize,

    /// The interval to fetch block and update the snapshot tree.
    /// Default: 5 seconds
    ///
    /// This interval is used to fetch block from rpc client.
    #[arg(long, value_parser = parse_duration, default_value = "5")]
    update_interval: std::time::Duration,

    /// The timeout for rpc server.
    /// Default: 10 seconds
    ///
    /// This timeout is used to set the timeout for rpc server.
    #[arg(long, value_parser = parse_duration, default_value = "10")]
    rpc_timeout: std::time::Duration,

    /// The address for prometheus server.
    /// Default: ""
    ///
    /// This address is used for the prometheus server.
    #[arg(long, default_value = "")]
    prometheus_addr: String,
}

fn parse_duration(arg: &str) -> Result<std::time::Duration, std::num::ParseIntError> {
    let seconds = arg.parse()?;
    Ok(std::time::Duration::from_secs(seconds))
}

fn parse_chain_cfg(arg: &str) -> Result<CfgEnv> {
    let mut chain_cfg = CfgEnv::default();
    if arg.is_empty() || arg == "eth" {
        return Ok(chain_cfg);
    }
    if arg == "seth" {
        chain_cfg.chain_id = 11155111;
        return Ok(chain_cfg);
    }
    if arg == "polygon" {
        chain_cfg.chain_id = 137;
        return Ok(chain_cfg);
    }
    if arg == "linea" {
        chain_cfg.chain_id = 59144;
        return Ok(chain_cfg);
    }
    #[cfg(feature = "optimism")]
    {
        if arg == "op" {
            chain_cfg.chain_id = 10;
            return Ok(chain_cfg);
        } else {
            // for opstack chains
            let chain_id = arg.parse::<u64>()?;
            chain_cfg.chain_id = chain_id;
            return Ok(chain_cfg);
        }
    }
    #[cfg(not(feature = "optimism"))]
    {
        use serde_json::from_str;
        use std::fs;
        let path = PathBuf::from(arg);
        if !path.exists() {
            bail!("chain config file not exists");
        }
        let data = fs::read_to_string(path.as_path())?;
        let chain_cfg = from_str(&data)?;
        Ok(chain_cfg)
    }
}

impl Command {
    async fn start(
        &self,
        chain_cfg: CfgEnv,
    ) -> Result<(
        tokio::sync::watch::Sender<()>,
        jsonrpsee::server::ServerHandle,
        tokio::sync::watch::Sender<()>,
    )> {
        match self.db_type.as_str() {
            "rocksdb" => {
                let db = Arc::new(RocksDBStorage::open(self.db_path.as_path(), self.db_cache));
                let mut metrics_handle = tokio::sync::watch::channel(()).0;
                if self.prometheus_addr.len() > 0 {
                    metrics_handle =
                        metrics::prometheus_build(db.clone(), self.prometheus_addr.clone());
                }
                let db = StateDBWrapper(db);
                let snaps = Arc::new(SnapshotTree::new(
                    db,
                    SnapshotTreeConfig::new(
                        self.diff_depth_limit,
                        self.account_cache_size,
                        self.storage_cache_size,
                        self.code_cache_size,
                    ),
                )?);
                info!(target:"updater", "start leafage server at {}, max_connections: {}", self.listen_addr, self.max_connections);
                let rpc_handle = ApiBuilder::new(snaps.clone(), chain_cfg.clone())
                    .build_and_run(&self.listen_addr, self.max_connections, self.rpc_timeout)
                    .await?;
                if let Some(rpc_address) = self.rpc_addr.clone() {
                    let updater = Updater::new(snaps.clone(), rpc_address, self.update_interval)?;
                    let updater_handle = updater.start();
                    return Ok((updater_handle, rpc_handle, metrics_handle));
                }
                Ok((
                    tokio::sync::watch::channel(()).0,
                    rpc_handle,
                    metrics_handle,
                ))
            }
            _ => bail!("only support rocksdb"),
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        let (updater_handle, rpc_handle, metrics_handle) =
            self.start(self.chain_cfg.clone()).await?;
        run_until_ctrl_c(async move {
            info!("stopping leafage server...");
            let _ = updater_handle.send(());
            let _ = rpc_handle.stop();
            let _ = metrics_handle.send(());
            Ok(())
        })
        .await?;
        Ok(())
    }
}
