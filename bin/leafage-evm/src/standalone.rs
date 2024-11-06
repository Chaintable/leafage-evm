use crate::initializer::initialize_check;
use crate::metrics;
use crate::runner::run_until_ctrl_c;
use crate::updater::updater_build;
use crate::utils::KafkaS3Config;
use anyhow::{bail, Result};
use clap::Parser;
use leafage_evm_rpc::ApiBuilder;
use leafage_evm_storage::{
    ArchiveRocksDBStorage, ArchiveTree, RocksDBStorage, SnapshotTree, SnapshotTreeConfig,
    StateDBWrapper,
};
use revm::primitives::{CfgEnv, SpecId};
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

    /// The Ethereum Execution Specification ID for the chain.
    ///
    /// if not specified, the default spec_id is u8::MAX
    #[arg(long, default_value = "255")]
    spec_id: u8,

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
    /// Default: 100 milliseconds
    ///
    /// This interval is used to fetch block from rpc client.
    #[arg(long, value_parser = parse_duration, default_value = "100")]
    update_interval: std::time::Duration,

    /// The timeout for rpc server.
    /// Default: 10 seconds
    ///
    /// This timeout is used to set the timeout for rpc server.
    #[arg(long, value_parser = parse_duration, default_value = "10000")]
    rpc_timeout: std::time::Duration,

    /// The address for prometheus server.
    /// Default: ""
    ///
    /// This address is used for the prometheus server.
    #[arg(long, default_value = "")]
    prometheus_addr: String,

    /// Whether to presist the history of the state
    /// Default: false
    ///
    /// This flag is used to enable the history of the state.
    #[arg(long, default_value_t = false)]
    archive: bool,

    /// The kafka s3 config
    /// Default: None
    ///
    /// This config is used to set the kafka s3 config.
    #[arg(long, value_parser = parse_kafka_s3_config)]
    kafka_s3_config: Option<KafkaS3Config>,
}

fn parse_duration(arg: &str) -> Result<std::time::Duration, std::num::ParseIntError> {
    let millis = arg.parse()?;
    Ok(std::time::Duration::from_millis(millis))
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
    if arg == "op" {
        chain_cfg.chain_id = 10;
        return Ok(chain_cfg);
    }
    if arg.parse::<u64>().is_ok() {
        chain_cfg.chain_id = arg.parse().unwrap();
        return Ok(chain_cfg);
    }
    let chain_cfg = serde_json::from_str(arg)?;
    Ok(chain_cfg)
}

fn parse_kafka_s3_config(arg: &str) -> Result<KafkaS3Config> {
    let file = std::fs::File::open(arg)?;
    let kafka_s3_config: KafkaS3Config = serde_json::from_reader(file)?;
    Ok(kafka_s3_config)
}

impl Command {
    async fn start(
        &self,
        chain_cfg: CfgEnv,
        spec_id: SpecId,
    ) -> Result<(
        tokio::sync::watch::Sender<()>,
        jsonrpsee::server::ServerHandle,
        tokio::sync::watch::Sender<()>,
    )> {
        info!(target:"updater", "chain cfg: {:?}, spec_id: {:?}, archive: {:?}", chain_cfg, spec_id, self.archive);
        info!(target:"updater", "start leafage server at {}, max_connections: {}, update_interval {:?}", self.listen_addr, self.max_connections, self.update_interval);
        match self.db_type.as_str() {
            "rocksdb" if !self.archive => {
                let db = Arc::new(RocksDBStorage::open(self.db_path.as_path(), self.db_cache));
                let metrics_handle =
                    metrics::prometheus_build(db.clone(), self.prometheus_addr.clone());
                let tree = Arc::new(SnapshotTree::new(
                    StateDBWrapper(db),
                    SnapshotTreeConfig::new(
                        self.diff_depth_limit,
                        self.account_cache_size,
                        self.storage_cache_size,
                        self.code_cache_size,
                    ),
                )?);
                let rpc_handle = ApiBuilder::new(tree.clone(), chain_cfg.clone(), spec_id)
                    .build_and_run(&self.listen_addr, self.max_connections, self.rpc_timeout)
                    .await?;

                let updater_handle = updater_build(
                    tree.clone(),
                    self.rpc_addr.clone(),
                    self.kafka_s3_config.clone(),
                    self.update_interval,
                )
                .await?;
                Ok((updater_handle, rpc_handle, metrics_handle))
            }
            "rocksdb" if self.archive => {
                let db = ArchiveRocksDBStorage::open(self.db_path.as_path(), self.db_cache);
                let metrics_handle =
                    metrics::prometheus_build(db.clone(), self.prometheus_addr.clone());
                // check if db shoud be initialized
                initialize_check(
                    StateDBWrapper(db.clone()),
                    self.rpc_addr.clone(),
                    self.kafka_s3_config.clone(),
                )
                .await?;

                let tree = Arc::new(ArchiveTree::new(
                    db,
                    SnapshotTreeConfig::new(
                        self.diff_depth_limit,
                        self.account_cache_size,
                        self.storage_cache_size,
                        self.code_cache_size,
                    ),
                )?);
                let rpc_handle = ApiBuilder::new(tree.clone(), chain_cfg.clone(), spec_id)
                    .build_and_run(&self.listen_addr, self.max_connections, self.rpc_timeout)
                    .await?;

                let updater_handle = updater_build(
                    tree.clone(),
                    self.rpc_addr.clone(),
                    self.kafka_s3_config.clone(),
                    self.update_interval,
                )
                .await?;
                Ok((updater_handle, rpc_handle, metrics_handle))
            }
            _ => bail!("only support rocksdb"),
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        let (updater_handle, rpc_handle, metrics_handle) = self
            .start(
                self.chain_cfg.clone(),
                SpecId::try_from_u8(self.spec_id).unwrap_or(SpecId::LATEST),
            )
            .await?;
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
