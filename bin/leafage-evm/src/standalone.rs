use crate::initializer::initialize_check;
use crate::pprof::PProf;
use crate::register::register_build;
use crate::runner::run_until_ctrl_c;
use crate::updater::updater_build;
use crate::utils::{EtcdRegisterConfig, KafkaS3Config};
use crate::warm::Warmup;
use anyhow::{bail, Result};
use clap::Parser;
#[cfg(target_os = "linux")]
use leafage_evm_rpc::InterceptorConfig;
use leafage_evm_rpc::{ApiBuilder, MultiChainCfgEnv};
use leafage_evm_storage::{
    MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig, StorageKind,
};
use leafage_evm_types::{Address, BlockId, BlockNumberOrTag};
use metrics::gauge;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::time;
use tracing::info;

/// `leafage-evm standalone` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to Cfg config to use for this node.
    ///
    /// If not specified, the default config [eth] will be used.
    #[arg(long, value_parser = parse_chain_cfg, default_value = "1")]
    chain_cfg: u64,

    /// The type of evm to use for this node.
    /// Default: mainnet
    #[arg(long, value_parser = ["mainnet", "op", "bsc", "cosmos"], default_value = "mainnet")]
    evm_type: String,

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
    #[arg(long, value_parser = ["rocksdb", "mdbx"],default_value = "rocksdb")]
    db_type: StorageKind,

    /// The size of the database cache in MB.
    /// Default: 2048
    ///
    /// This limit is used for the rocksdb cache.
    #[arg(long, default_value = "2048")]
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
    /// Default: 2000000
    ///
    /// This limit is used for the storage cache.
    #[arg(long, default_value = "2000000")]
    storage_cache_size: usize,

    /// The size of the code cache.
    /// Default: 200000
    ///
    /// This limit is used for the code cache.
    #[arg(long, default_value = "200000")]
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

    /// The wait timeout for stoping the server.
    /// Default: 5 seconds
    ///
    /// This timeout is used to wait for the etcd unregister to complete.
    #[arg(long, default_value = "5")]
    stop_wait_timeout: u64,

    /// Whether to normalize the state key
    /// Default: false
    ///
    /// This flag is used to enable the normalize state key.
    /// only avax chain need to enable this flag.
    #[arg(long, default_value_t = false)]
    normalize_state_key: bool,

    /// The address for readiness probe server.
    /// Default: ""
    ///
    /// This address is used for the readiness probe server.
    #[arg(long, default_value = "")]
    readiness_addr: String,

    /// Number of warmup blocks
    /// Default: 0
    ///
    /// This is only used when `readiness_addr` is set.
    #[arg(long, default_value = "0")]
    warmup_blocks: usize,

    /// Number of warmup tokens
    /// Default: 0
    ///
    /// This is only used when `readiness_addr` is set.
    #[arg(long, default_value = "0")]
    warmup_tokens: usize,

    /// The address for pprof server.
    /// Default: ""
    ///
    /// This address is used for the pprof server.
    #[arg(long, default_value = "")]
    pprof_addr: String,
}

fn parse_duration(arg: &str) -> Result<std::time::Duration, std::num::ParseIntError> {
    let millis = arg.parse()?;
    Ok(std::time::Duration::from_millis(millis))
}

fn parse_chain_cfg(arg: &str) -> Result<u64> {
    if arg.is_empty() || arg == "eth" {
        return Ok(1);
    }
    if arg == "seth" {
        return Ok(11155111);
    }
    if arg == "polygon" {
        return Ok(137);
    }
    if arg == "linea" {
        return Ok(59144);
    }
    if arg == "op" {
        return Ok(10);
    }
    if arg.parse::<u64>().is_ok() {
        return Ok(arg.parse().unwrap());
    } else {
        bail!("invalid chain cfg: {}", arg);
    }
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
        chain_cfg: MultiChainCfgEnv,
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
                .add_global_label("chain_id", format!("{}", chain_cfg.chain_id()))
                .install()?;
            let labels = [("role", "replica".to_string())];
            let gauge = gauge!("pipeline_node_info", &labels);
            gauge.set(1.0);
        }
        let ready = Arc::new(AtomicBool::new(false));
        if !self.readiness_addr.is_empty() {
            let readiness_addr = self.readiness_addr.parse::<std::net::SocketAddr>()?;
            info!(target: "updater", "starting readiness server on {}", readiness_addr);

            let handle = ready.clone();

            tokio::spawn(async move {
                let app = axum::Router::new().route(
                    "/",
                    axum::routing::get(move || async move {
                        if handle.load(std::sync::atomic::Ordering::SeqCst) {
                            (axum::http::StatusCode::OK, "ready")
                        } else {
                            (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not ready")
                        }
                    }),
                );

                let listener = tokio::net::TcpListener::bind(readiness_addr)
                    .await
                    .expect("Failed to bind readiness server");

                axum::serve(listener, app)
                    .await
                    .expect("Failed to serve readiness");
            });
        }

        if !self.pprof_addr.is_empty() {
            let pprof_server = PProf::new(self.pprof_addr.parse()?);
            tokio::spawn(async move {
                pprof_server
                    .start()
                    .await
                    .expect("Failed to start pprof server");
            });
        }

        let mut etcd_config = self.etcd_config.clone();
        if etcd_config.is_some() && !self.meta.is_empty() {
            etcd_config.as_mut().unwrap().meta = self.meta.clone();
        }

        // set default offset dir if not set
        if let Some(kafka_s3_config) = &mut self.kafka_s3_config {
            if kafka_s3_config.offset_dir.is_empty() {
                kafka_s3_config.offset_dir =
                    format!("{}/offset", self.db_path.to_str().unwrap_or_default());
            }
            info!(target:"updater", "kafka s3 config: {:?}", kafka_s3_config);
        } else {
            info!(target:"updater", "no kafka s3 config");
        }

        let db = MultiStorage::open(
            self.db_path.as_path(),
            self.db_cache,
            self.db_type,
            self.archive,
        )?;

        // check if db shoud be initialized
        initialize_check(
            StateDBWrapper(
                db.db_at(BlockId::Number(BlockNumberOrTag::Latest))?
                    .unwrap(),
            ),
            self.rpc_addr.clone(),
            self.kafka_s3_config.clone(),
            self.genesis_number,
        )
        .await?;

        let tree = Arc::new(StateTree::new(
            db,
            StateTreeConfig::new(
                self.diff_depth_limit,
                self.account_cache_size,
                self.storage_cache_size,
                self.code_cache_size,
            ),
        )?);

        let mut rpc_builder = ApiBuilder::new(tree.clone(), chain_cfg.clone())
            .with_historical_config(self.historical_rpc.clone(), self.historical_height);

        if !self.readiness_addr.is_empty() {
            let warmup = Warmup::new(
                self.rpc_addr.clone(),
                self.kafka_s3_config.clone().unwrap_or_default(),
                tree.clone(),
                self.warmup_blocks,
                self.warmup_tokens,
                self.init_task_queue_size,
            )
            .await?;
            rpc_builder = warmup.with_warmup_data(rpc_builder).await;
        }

        let rpc_handle = rpc_builder
            .build_and_run(
                &self.listen_addr,
                self.max_connections,
                self.rpc_timeout,
                #[cfg(target_os = "linux")]
                self.interceptor_config.clone(),
                self.ovm_address.clone(),
                self.archive,
                self.normalize_state_key,
                self.kafka_s3_config.clone().unwrap_or_default().version,
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

        let registry_handle = register_build(
            chain_cfg.chain_id(),
            self.kafka_s3_config.clone().unwrap_or_default().version,
            etcd_config.clone(),
            self.archive,
        )
        .await?;

        ready.store(true, std::sync::atomic::Ordering::SeqCst);
        info!(target:"updater", "leafage server started");

        Ok((updater_handle, rpc_handle, registry_handle))
    }

    pub async fn run(&mut self) -> Result<()> {
        let (updater_handle, rpc_handle, resgitry_handle) = self
            .start((self.chain_cfg.clone(), self.evm_type.clone()).into())
            .await?;
        run_until_ctrl_c(async move {
            info!("stopping leafage server...");
            let _ = updater_handle.send(());
            let _ = resgitry_handle.send(());
            // wait for lease to unregist
            info!(
                "waiting for etcd lease to expire in {} seconds...",
                self.stop_wait_timeout
            );
            time::sleep(std::time::Duration::from_secs(
                self.stop_wait_timeout as u64,
            ))
            .await;
            let _ = rpc_handle.stop();
            Ok(())
        })
        .await?;
        Ok(())
    }
}
