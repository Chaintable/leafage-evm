use crate::initializer::initialize_check;
use crate::pprof::PProf;
use crate::register::register_build;
use crate::runner::run_until_ctrl_c;
use crate::updater::updater_build;
use crate::utils::{parse_kafka_s3_config, EtcdRegisterConfig, KafkaS3Config, NodeTypeArg};
use crate::warm::Warmup;
use anyhow::{anyhow, bail, Result};
use clap::Parser;
use leafage_evm_chains::base::BaseHardfork;
use leafage_evm_chains::citrea::CitreaHardfork;
#[cfg(target_os = "linux")]
use leafage_evm_rpc::InterceptorConfig;
use leafage_evm_rpc::{ApiBuilder, MultiChainCfgEnv, TokenCollector};
use leafage_evm_storage::{
    MultiStorage, StateDBProvider, StateDBWrapper, StateTree, StateTreeConfig, StorageKind,
};
use leafage_evm_types::{Address, BlockId, BlockNumberOrTag, CfgEnv, MainnetSpecId, OpSpecId};
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
    #[arg(
        long,
        value_parser = [
            "mainnet",
            "arbitrum",
            "op",
            "base",
            "bsc",
            "cosmos",
            "mantlev2",
            "tempo",
            "citrea",
            "iotex",
            "moonbeam",
            "moonriver",
            "polygon",
        ],
        default_value = "mainnet"
    )]
    evm_type: String,

    /// Custom EVM parameters. Currently, this only supports the **Cosmos** ecosystem.
    ///
    /// # Example
    /// --evm-type=cosmos
    /// --evm-custom-config={"native_token":"0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}
    #[arg(long)]
    evm_custom_config: Option<String>,

    /// The Ethereum Execution Specification ID for the chain.
    ///
    /// if not specified, the default spec_id is u8::MAX
    #[arg(long, default_value = "255")]
    spec_id: u8,

    /// Maximum gas limit for RPC methods
    /// [default: 100000000]
    #[arg(long, default_value_t = 100000000)]
    pub rpc_gas_cap: u64,

    /// The path to the database to use for this node.
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// The type of database to use for this node.
    /// Default: rocksdb
    #[arg(long, default_value = "rocksdb")]
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

    /// The TCP accept-queue backlog for the HTTP-RPC listener.
    /// Default: 4096
    ///
    /// The kernel caps the effective queue at `min(this, net.core.somaxconn)`.
    /// A too-small backlog drops completing handshakes under connection bursts
    /// (kernel `ListenOverflows`), which upstream proxies observe as
    /// `dial ...: i/o timeout`. Keep this <= the pod's `net.core.somaxconn`.
    #[arg(long, default_value = "4096")]
    listen_backlog: u32,

    /// The depth limit of the diff tree.
    /// Default: 64 for eth mainnet
    ///
    /// This limit is finalized block number - current block number.
    #[arg(long, default_value = "64")]
    diff_depth_limit: usize,

    /// The reorg buffer depth for S3 catch-up.
    ///
    /// During S3 catch-up the by-number index can resolve a wrong branch
    /// around the chain tip while a reorg is in flight, leaving the hand-off
    /// block disconnected from the Kafka stream. With a non-zero value, the
    /// last `catchup_safe_depth` blocks below the Kafka head are backfilled by
    /// following the exact parent-hash chain instead of the by-number index.
    /// Set it above the chain's maximum reorg depth (e.g. 64 for Moonriver).
    ///
    /// Default: 0 (disabled; identical to the legacy by-number-only behavior).
    #[arg(long, default_value = "0")]
    catchup_safe_depth: usize,

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

    /// Disables db automatic compactions.
    /// Default: false
    /// This value is used when `db_type` is Rocksdb.
    #[arg(long, default_value = "false")]
    disable_auto_compactions: bool,

    /// Use ZSTD-with-dict compression at deep levels for the three large
    /// archive CFs (BlockHashToBlockInfo, AddressToAccount, AddressToStorage).
    /// Default: false (uniform LZ4 — same as the pre-refactor build).
    ///
    /// When enabled, archive disk usage shrinks by ~15-20% over time as
    /// compaction rewrites deep levels, at the cost of ~2× compaction CPU and
    /// ~3× cold-read decompression latency on deep-level reads. RocksDB
    /// records compression type per SST, so existing data remains readable
    /// regardless of this flag — toggling only affects newly written SSTs.
    /// Archive-only (RocksDB archive mode).
    #[arg(long, default_value = "false")]
    archive_zstd_compression: bool,

    /// Iterator timeout in seconds for archive mode.
    /// Default: 0 (disabled)
    /// When > 0, StateDB iterators will be tracked and logged when they exceed this timeout.
    /// At 2x timeout, iterators are force-released to unblock RocksDB compaction.
    #[arg(long, default_value = "0")]
    iterator_timeout_secs: u64,

    /// Size (MB) of the in-memory content-addressed code cache
    /// (code_hash → code) for archive reads. Serves debank_getAddressCode /
    /// eth_getCode / multicall code loads at any block height without
    /// touching the HashToCode CF. 0 disables.
    /// Archive-only (RocksDB archive mode).
    #[arg(long, env = "ARCHIVE_CODE_CACHE_MB", default_value = "256")]
    archive_code_cache_mb: u64,

    /// Gas estimation buffer percentage (100 = no buffer, 120 = +20% buffer)
    /// Default: 100
    ///
    /// This adds a safety margin to gas estimates to reduce the risk of out-of-gas errors.
    #[arg(long, default_value = "100")]
    estimate_gas_buffer: u64,

    /// Path to the local JSON file for auto-collecting ERC20 token addresses.
    /// Default: "" (disabled)
    ///
    /// When set, ERC20 contract addresses observed in eth_call / contractMultiCall
    /// will be automatically saved to this file for future warmup use.
    #[arg(long, default_value = "")]
    token_collector_path: String,

    /// The node type to register to etcd.
    /// Default: auto (derive from --archive)
    ///
    /// `auto` registers archive nodes as archive and all others as state.
    /// `state` / `archive` override that explicitly, regardless of --archive.
    #[arg(long, value_enum, default_value_t = NodeTypeArg::Auto)]
    node_type: NodeTypeArg,

    /// Use the inverted (descending) block-height key encoding for archive
    /// account/storage reads and writes.
    /// Default: false (legacy ascending encoding)
    ///
    /// The descending encoding turns historical reads into forward seeks that
    /// use the prefix bloom (much faster `eth_call`). It is a different on-disk
    /// key layout: only enable this against an archive DB built (via
    /// `archive-init` / re-sync) with the same flag — mixing layouts silently
    /// returns wrong values. Has no effect in state-node (non-archive) mode.
    #[arg(long, default_value_t = false)]
    inverted_block_encoding: bool,
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
    if arg == "tempo" {
        return Ok(4217);
    }
    if arg.parse::<u64>().is_ok() {
        return Ok(arg.parse().unwrap());
    } else {
        bail!("invalid chain cfg: {}", arg);
    }
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

/// Resolve `--spec-id` to a typed EVM spec; `u8::MAX` (CLI default) → keep evm-type's built-in spec.
fn resolve_spec<T: TryFrom<u8>>(spec_id: u8, default: T, type_label: &str) -> Result<T> {
    if spec_id == u8::MAX {
        return Ok(default);
    }
    T::try_from(spec_id)
        .map_err(|_| anyhow!("invalid --spec-id {} for {} evm-type", spec_id, type_label))
}

impl Command {
    fn build_chain_cfg_env(&self) -> Result<MultiChainCfgEnv> {
        let chain_id = self.chain_cfg;
        let evm_type = self.evm_type.clone();
        let custom_evm_cfg = self.evm_custom_config.clone();
        let gas_cap = self.rpc_gas_cap;
        match evm_type.as_str() {
            "mainnet" => {
                let spec = resolve_spec(self.spec_id, MainnetSpecId::AMSTERDAM, "mainnet")?;
                let mut chain_cfg = CfgEnv::new_with_spec(spec);
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                chain_cfg.tx_gas_limit_cap = Some(gas_cap);
                Ok(MultiChainCfgEnv::Mainnet(chain_cfg))
            }
            "arbitrum" => {
                // Arbitrum Orbit (Nitro) on ArbOS >= 40 is Prague-level. Its EIP-7623
                // calldata floor is a runtime feature flag (default off, e.g. Robinhood),
                // so default to PRAGUE: it matches the chain's Prague EVM and never
                // *under*-estimates the floor — if a chain enables 7623 PRAGUE is exact,
                // if it's off the floor only over-estimates rare calldata-heavy txs (the
                // safe direction). Override with --spec-id for pre-Prague (ArbOS < 40)
                // chains. The L1 cost is added separately in estimate_l1_overhead.
                let spec = resolve_spec(self.spec_id, MainnetSpecId::PRAGUE, "arbitrum")?;
                let mut chain_cfg = CfgEnv::new_with_spec(spec);
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                chain_cfg.tx_gas_limit_cap = Some(gas_cap);
                let custom_evm_cfg = custom_evm_cfg
                    .map(|str| {
                        serde_json::from_str(&str).map_err(|err| {
                            anyhow!("cannot parse arbitrum custom evm config: {}", err)
                        })
                    })
                    .transpose()?;
                Ok(MultiChainCfgEnv::Arbitrum((chain_cfg, custom_evm_cfg)))
            }
            "op" => {
                let mut chain_cfg = CfgEnv::new_with_spec(OpSpecId::OSAKA);
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                chain_cfg.tx_gas_limit_cap = Some(gas_cap);
                Ok(MultiChainCfgEnv::Op(chain_cfg))
            }
            "base" => {
                // Base forked from the OP stack; execution is OP-equivalent
                // (Beryl precompiles are layered on separately).
                let mut chain_cfg = CfgEnv::new_with_spec(BaseHardfork::from(OpSpecId::OSAKA));
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                Ok(MultiChainCfgEnv::Base(chain_cfg))
            }
            "bsc" => {
                let mut chain_cfg = CfgEnv::default();
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                chain_cfg.tx_gas_limit_cap = Some(gas_cap);
                Ok(MultiChainCfgEnv::Bsc(chain_cfg))
            }
            "cosmos" => {
                let mut chain_cfg = CfgEnv::new_with_spec(MainnetSpecId::AMSTERDAM.into());
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                chain_cfg.tx_gas_limit_cap = Some(gas_cap);
                let custom_evm_cfg = custom_evm_cfg
                    .map(|str| {
                        serde_json::from_str(&str).map_err(|err| {
                            anyhow!("cannot parse cosmos custom evm config: {}", err)
                        })
                    })
                    .transpose()?;
                Ok(MultiChainCfgEnv::Cosmos((chain_cfg, custom_evm_cfg)))
            }
            "iotex" => {
                let spec = resolve_spec(self.spec_id, MainnetSpecId::AMSTERDAM, "iotex")?;
                let mut chain_cfg = CfgEnv::new_with_spec(spec.into());
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                chain_cfg.tx_gas_limit_cap = Some(gas_cap);
                Ok(MultiChainCfgEnv::Iotex(chain_cfg))
            }
            "polygon" => {
                let spec = resolve_spec(
                    self.spec_id,
                    leafage_evm_chains::polygon::PolygonHardfork::default(),
                    "polygon",
                )?;
                let mut chain_cfg = CfgEnv::new_with_spec(spec);
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                chain_cfg.tx_gas_limit_cap = Some(gas_cap);
                Ok(MultiChainCfgEnv::Polygon(chain_cfg))
            }
            // Moonbeam and Moonriver share an identical EVM and precompile set;
            // they differ only by chain id (passed via --chain-cfg) and native
            // token metadata, which leafage does not need. Both map to the same
            // MoonbeamHardfork executor.
            "moonbeam" | "moonriver" => {
                let spec = resolve_spec(self.spec_id, MainnetSpecId::AMSTERDAM, &evm_type)?;
                let mut chain_cfg = CfgEnv::new_with_spec(spec.into());
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                chain_cfg.tx_gas_limit_cap = Some(gas_cap);
                Ok(MultiChainCfgEnv::Moonbeam(chain_cfg))
            }
            "mantlev2" => {
                let mut chain_cfg = CfgEnv::new_with_spec(OpSpecId::OSAKA.into());
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                chain_cfg.tx_gas_limit_cap = Some(gas_cap);
                Ok(MultiChainCfgEnv::Mantle(chain_cfg))
            }
            "tempo" => {
                let mut chain_cfg = CfgEnv::new_with_spec(
                    leafage_evm_chains::tempo::hardfork::TempoHardfork::default(),
                );
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                chain_cfg.tx_gas_limit_cap = Some(gas_cap);
                Ok(MultiChainCfgEnv::Tempo(chain_cfg))
            }
            "citrea" => {
                let mut chain_cfg =
                    CfgEnv::new_with_spec(CitreaHardfork::from(MainnetSpecId::AMSTERDAM));
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                chain_cfg.tx_gas_limit_cap = Some(gas_cap);
                Ok(MultiChainCfgEnv::Citrea(chain_cfg))
            }
            _ => bail!("Unsupported evm type"),
        }
    }
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
            // Latency buckets (seconds) so `leafage_rpc_call_time` exports as a
            // Prometheus `_bucket`/`_count`/`_sum` histogram (queryable via
            // `histogram_quantile(... by (method_name, le))`) instead of the
            // exporter's default summary rendering. This replaces the previous
            // `{quantile=...}` series; `_count`/`_sum` are unchanged.
            const LATENCY_BUCKETS: &[f64] = &[
                0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
                10.0,
            ];
            metrics_exporter_prometheus::PrometheusBuilder::new()
                .with_http_listener(self.prometheus_addr.parse::<std::net::SocketAddr>()?)
                .add_global_label("chain_id", format!("{}", chain_cfg.chain_id()))
                .set_buckets_for_metric(
                    metrics_exporter_prometheus::Matcher::Full("leafage_rpc_call_time".to_string()),
                    LATENCY_BUCKETS,
                )?
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

        // Set iterator timeout environment variable for archive mode
        if self.archive {
            std::env::set_var(
                "ROCKSDB_ITERATOR_TIMEOUT_SECS",
                self.iterator_timeout_secs.to_string(),
            );
            std::env::set_var(
                "ARCHIVE_CODE_CACHE_MB",
                self.archive_code_cache_mb.to_string(),
            );
        }

        let db = MultiStorage::open(
            self.db_path.as_path(),
            self.db_cache,
            self.db_type,
            self.archive,
            self.disable_auto_compactions,
            self.archive_zstd_compression,
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

        // MDBX has per-handle ro_txn snapshots; the shared CacheDiskLayer
        // would blur those boundaries, so disable it for MDBX backends.
        let enable_cache = matches!(self.db_type, StorageKind::Rocksdb);
        let tree = Arc::new(StateTree::new(
            db,
            StateTreeConfig::new(
                self.diff_depth_limit,
                self.account_cache_size,
                self.storage_cache_size,
                self.code_cache_size,
                enable_cache,
            ),
        )?);

        let mut rpc_builder = ApiBuilder::new(tree.clone(), chain_cfg.clone())
            .with_ovm_address(self.ovm_address)
            .with_historical_config(self.historical_rpc.clone(), self.historical_height);

        #[cfg(target_os = "linux")]
        {
            rpc_builder = rpc_builder.with_interceptor_cfg(self.interceptor_config.clone());
        }
        if !self.readiness_addr.is_empty() {
            // Initialize token collector if path is configured (before warmup so it can be used)
            let token_collector_path = if !self.token_collector_path.is_empty() {
                let collector_path = PathBuf::from(&self.token_collector_path);
                if let Some(parent) = collector_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                collector_path
            } else {
                self.db_path.join("tokens.json")
            };
            info!(target: "updater", "token collector enabled, saving to {:?}", token_collector_path);
            let token_collector = TokenCollector::new(token_collector_path).await?;
            let warmup = Warmup::new(
                self.rpc_addr.clone(),
                self.kafka_s3_config.clone().unwrap_or_default(),
                tree.clone(),
                self.warmup_blocks,
                self.warmup_tokens,
                self.init_task_queue_size,
                token_collector.clone(),
            )
            .await?;
            rpc_builder = warmup.with_warmup_data(rpc_builder).await;
            rpc_builder = rpc_builder.with_token_collector(token_collector);
        }

        let rpc_handle = rpc_builder
            .build_and_run(
                &self.listen_addr,
                self.max_connections,
                self.rpc_timeout,
                self.archive,
                self.normalize_state_key,
                self.kafka_s3_config.clone().unwrap_or_default().version,
                self.estimate_gas_buffer,
                self.listen_backlog,
            )
            .await?;

        let updater_handle = updater_build(
            tree.clone(),
            self.rpc_addr.clone(),
            self.kafka_s3_config.clone(),
            self.update_interval,
            self.diff_depth_limit,
            self.init_task_queue_size,
            self.catchup_safe_depth,
        )
        .await?;

        let registry_handle = register_build(
            chain_cfg.chain_id(),
            self.kafka_s3_config.clone().unwrap_or_default().version,
            etcd_config.clone(),
            self.node_type.resolve(self.archive),
        )
        .await?;

        ready.store(true, std::sync::atomic::Ordering::SeqCst);
        info!(target:"updater", "leafage server started");

        Ok((updater_handle, rpc_handle, registry_handle))
    }

    pub async fn run(&mut self) -> Result<()> {
        // Fix the versioned-key encoding mode before any archive DB access.
        leafage_evm_storage::set_inverted_block_encoding(self.inverted_block_encoding);
        let (updater_handle, rpc_handle, resgitry_handle) =
            self.start(self.build_chain_cfg_env()?).await?;
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
