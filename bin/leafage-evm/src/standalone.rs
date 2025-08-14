use crate::initializer::initialize_check;
use crate::register::register_build;
use crate::runner::run_until_ctrl_c;
use crate::updater::updater_build;
use crate::utils::{EtcdRegisterConfig, KafkaS3Config};
use anyhow::{bail, Result};
use clap::Parser;
use leafage_evm_rpc::ApiBuilder;
#[cfg(target_os = "linux")]
use leafage_evm_rpc::InterceptorConfig;
use leafage_evm_storage::{
    ArchiveRocksDBStorage, ArchiveTree, RocksDBStorage, SnapshotTree, SnapshotTreeConfig,
    StateDBWrapper,
};
use leafage_evm_types::{Address, CfgEnv, SpecId};
use metrics::gauge;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tracing::info;

/// `leafage-evm standalone` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to Cfg config to use for this node.
    ///
    /// If not specified, the default config [eth] will be used.
    #[arg(long, value_parser = parse_chain_cfg, default_value = "eth")]
    chain_cfg: CfgEnv<SpecId>,

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

    /// The kafka s3 config path
    /// Default: None
    ///
    /// This config is used to set the kafka s3 config.
    #[arg(long, value_parser = parse_kafka_s3_config,  value_name = "KAFKA_S3_CONFIG_PATH")]
    kafka_s3_config: Option<KafkaS3Config>,

    /// The etcd register config path
    /// Default: None
    ///
    /// This config is used to set the etcd register config.
    #[arg(long, value_parser = parse_etcd_config, value_name = "ETCD_CONFIG_PATH")]
    etcd_config: Option<EtcdRegisterConfig>,

    /// The meta for node self
    /// Default: None
    ///
    /// This meta is used to set the meta for node self.
    #[arg(long, default_value = "")]
    meta: String,

    #[cfg(target_os = "linux")]
    /// The interceptor config path
    /// Default: None
    ///
    /// This config is used to set the interceptor config.
    ///
    #[arg(long, value_parser = parse_interceptor_config, value_name = "INTERCEPTOR_CONFIG_PATH")]
    interceptor_config: Option<InterceptorConfig>,

    /// The genesis number for the chain.
    /// Default: 0
    ///
    /// For some forked chains , the genesis block number is not 0, e.g. op-bedrock.
    #[arg(long, default_value = "0")]
    genesis_number: u64,

    /// The size of the task queue for the s3 updater.
    /// Default: 256
    ///
    /// This size is used to limit the number of async tasks in the queue.
    #[arg(long, default_value = "256")]
    init_task_queue_size: usize,

    /// Address for OVM
    /// Default: None
    ///
    /// This address is used to set the OVM address for the node.
    #[arg(long, value_parser = parse_ovm_address, value_name = "OVM_ADDRESS")]
    ovm_address: Option<Address>,

    /// Historical RPC endpoint for forwarding pre-fork requests
    #[arg(long, value_name = "URL")]
    historical_rpc: Option<String>,

    /// Fork height threshold for historical RPC forwarding
    #[arg(long)]
    historical_height: Option<u64>,
}

fn parse_duration(arg: &str) -> Result<std::time::Duration, std::num::ParseIntError> {
    let millis = arg.parse()?;
    Ok(std::time::Duration::from_millis(millis))
}

fn parse_chain_cfg(arg: &str) -> Result<CfgEnv<SpecId>> {
    let mut chain_cfg = CfgEnv::default();
    chain_cfg.disable_balance_check = true;
    chain_cfg.disable_eip3607 = true;
    chain_cfg.disable_block_gas_limit = true;
    chain_cfg.disable_base_fee = true;
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
    let kafka_s3_config: KafkaS3Config;
    if arg.starts_with("/") {
        let file = std::fs::File::open(arg)?;
        kafka_s3_config = serde_json::from_reader(file)?;
    } else {
        kafka_s3_config = serde_json::from_str(arg)?;
    }
    Ok(kafka_s3_config)
}

fn parse_etcd_config(arg: &str) -> Result<EtcdRegisterConfig> {
    let etcd_config: EtcdRegisterConfig;
    if arg.starts_with("/") {
        let file = std::fs::File::open(arg)?;
        etcd_config = serde_json::from_reader(file)?;
    } else {
        etcd_config = serde_json::from_str(arg)?;
    }
    Ok(etcd_config)
}

#[cfg(target_os = "linux")]
fn parse_interceptor_config(arg: &str) -> Result<InterceptorConfig> {
    let interceptor_config: InterceptorConfig;
    if arg.starts_with("/") {
        let file = std::fs::File::open(arg)?;
        interceptor_config = serde_json::from_reader(file)?;
    } else {
        interceptor_config = serde_json::from_str(arg)?;
    }
    Ok(interceptor_config)
}

fn parse_ovm_address(arg: &str) -> Result<Address> {
    if arg.is_empty() {
        bail!("ovm address cannot be empty");
    }
    let address = Address::from_str(arg)?;
    Ok(address)
}

impl Command {
    async fn start(
        &mut self,
        chain_cfg: CfgEnv<SpecId>,
    ) -> Result<(
        tokio::sync::watch::Sender<()>,
        jsonrpsee::server::ServerHandle,
        tokio::sync::watch::Sender<()>,
    )> {
        info!(target:"updater", "{:?}", self);
        info!(target:"updater", "start leafage server at {}, max_connections: {}, update_interval {:?}", self.listen_addr, self.max_connections, self.update_interval);
        if !self.prometheus_addr.is_empty() {
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .with_http_listener(self.prometheus_addr.parse::<std::net::SocketAddr>()?)
                .add_global_label("chain_id", format!("{}", chain_cfg.chain_id))
                .install()?;
            let labels = [("role", "replica".to_string())];
            let gauge = gauge!("pipeline_node_info", &labels);
            gauge.set(1.0);
        }
        let mut etcd_config = self.etcd_config.clone();
        if etcd_config.is_some() && !self.meta.is_empty() {
            etcd_config.as_mut().unwrap().meta = self.meta.clone();
        }
        let resgitry_handle =
            register_build(chain_cfg.chain_id, etcd_config.clone(), self.archive).await?;
        match self.db_type.as_str() {
            "rocksdb" if !self.archive => {
                let db = Arc::new(RocksDBStorage::open(self.db_path.as_path(), self.db_cache));
                let tree = Arc::new(SnapshotTree::new(
                    StateDBWrapper(db),
                    SnapshotTreeConfig::new(
                        self.diff_depth_limit,
                        self.account_cache_size,
                        self.storage_cache_size,
                        self.code_cache_size,
                    ),
                )?);
                let rpc_handle = ApiBuilder::new(tree.clone(), chain_cfg.clone())
                    .with_historical_config(self.historical_rpc.clone(), self.historical_height)
                    .build_and_run(
                        &self.listen_addr,
                        self.max_connections,
                        self.rpc_timeout,
                        #[cfg(target_os = "linux")]
                        self.interceptor_config.clone(),
                        self.ovm_address.clone(),
                    )
                    .await?;

                let updater_handle = updater_build(
                    tree.clone(),
                    self.rpc_addr.clone(),
                    self.kafka_s3_config.clone(),
                    self.update_interval,
                    self.diff_depth_limit,
                    self.init_task_queue_size,
                )
                .await?;
                Ok((updater_handle, rpc_handle, resgitry_handle))
            }
            "rocksdb" if self.archive => {
                let db = Arc::new(ArchiveRocksDBStorage::open(
                    self.db_path.as_path(),
                    self.db_cache,
                ));
                if let Some(kafka_s3_config) = &mut self.kafka_s3_config {
                    if kafka_s3_config.offset_dir.is_empty() {
                        kafka_s3_config.offset_dir =
                            format!("{}/offset", self.db_path.to_str().unwrap_or_default());
                    }
                    info!(target:"updater", "kafka s3 config: {:?}", kafka_s3_config);
                } else {
                    info!(target:"updater", "no kafka s3 config");
                }
                // check if db shoud be initialized
                initialize_check(
                    db.clone(),
                    self.rpc_addr.clone(),
                    self.kafka_s3_config.clone(),
                    self.genesis_number,
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
                let rpc_handle = ApiBuilder::new(tree.clone(), chain_cfg.clone())
                    .with_historical_config(self.historical_rpc.clone(), self.historical_height)
                    .build_and_run(
                        &self.listen_addr,
                        self.max_connections,
                        self.rpc_timeout,
                        #[cfg(target_os = "linux")]
                        self.interceptor_config.clone(),
                        self.ovm_address.clone(),
                    )
                    .await?;

                let updater_handle = updater_build(
                    tree.clone(),
                    self.rpc_addr.clone(),
                    self.kafka_s3_config.clone(),
                    self.update_interval,
                    self.diff_depth_limit,
                    self.init_task_queue_size,
                )
                .await?;
                Ok((updater_handle, rpc_handle, resgitry_handle))
            }
            _ => bail!("only support rocksdb"),
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        let (updater_handle, rpc_handle, resgitry_handle) =
            self.start(self.chain_cfg.clone()).await?;
        run_until_ctrl_c(async move {
            info!("stopping leafage server...");
            let _ = updater_handle.send(());
            let _ = rpc_handle.stop();
            let _ = resgitry_handle.send(());
            Ok(())
        })
        .await?;
        Ok(())
    }
}
