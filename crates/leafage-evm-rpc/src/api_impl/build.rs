use super::api_impl::NoneEvmCustomConfig;
use super::tempo::api::TempoEvmCustomConfig;
use super::token_collector::TokenCollector;
use super::ApiImpl;
#[cfg(target_os = "linux")]
use super::{InterceptorConfig, InterceptorLayer};
use crate::api::{BlockxApiServer, DebankApiServer, EthApiServer, PreApiServer};
use crate::api_impl::core::{
    Api, ApiBase, ApiCore, EvmExecutor, GetHaltReason, GetTransactionError, MultiChainCfgEnv,
    ToJsonRpcError,
};
use crate::api_impl::historical_overload::{HistoricalOverloadHttpLayer, HistoricalOverloadRpc};
use crate::metrics::{HttpMetricLayer, RpcMetric};
use jsonrpsee::server::{RpcServiceBuilder, ServerBuilder, ServerHandle};
use jsonrpsee::{
    http_client::{HttpClient, HttpClientBuilder},
    RpcModule,
};
use leafage_evm_chains::arc::ARC_MAINNET_CHAIN_ID;
use leafage_evm_storage::{BlockIndex, EvmStorageRead};
use leafage_evm_types::{Address, DebankErrorCode, DebankTransaction, PreErrorCode};
use std::time::Duration;
use tracing::error;

pub struct ApiBuilder<DB> {
    db: DB,
    cfg: MultiChainCfgEnv,
    ovm_address: Option<Address>,
    #[cfg(target_os = "linux")]
    interceptor_cfg: Option<InterceptorConfig>,
    historical_client: Option<HttpClient>,
    historical_height: Option<u64>,
    replay_blocks: Option<Vec<Vec<DebankTransaction>>>,
    warmup_erc20_addresses: Option<(Address, Vec<Address>)>,
    token_collector: Option<TokenCollector>,
    evm_exec_concurrency: usize,
    state_read_concurrency: usize,
}

impl<DB> ApiBuilder<DB>
where
    DB: EvmStorageRead + BlockIndex + Sync + Send + 'static,
{
    pub fn new(db: DB, cfg: MultiChainCfgEnv) -> Self {
        Self {
            db,
            cfg,
            ovm_address: None,
            #[cfg(target_os = "linux")]
            interceptor_cfg: None,
            historical_client: None,
            historical_height: None,
            replay_blocks: None,
            warmup_erc20_addresses: None,
            token_collector: None,
            evm_exec_concurrency: 0,
            state_read_concurrency: 0,
        }
    }

    pub fn with_ovm_address(mut self, ovm_address: Option<Address>) -> Self {
        self.ovm_address = ovm_address;
        self
    }

    #[cfg(target_os = "linux")]
    pub fn with_interceptor_cfg(mut self, interceptor_cfg: Option<InterceptorConfig>) -> Self {
        self.interceptor_cfg = interceptor_cfg;
        self
    }

    pub fn with_historical_config(
        mut self,
        historical_rpc: Option<String>,
        historical_height: Option<u64>,
    ) -> Self {
        if let Some(url) = historical_rpc {
            if let Ok(http_client) = HttpClientBuilder::default()
                .request_timeout(Duration::from_secs(30))
                .max_response_size(u32::MAX)
                .build(&url)
            {
                self.historical_client = Some(http_client);
            }
        }
        self.historical_height = historical_height;
        self
    }

    pub fn with_replay_blocks(mut self, blocks: Vec<Vec<DebankTransaction>>) -> Self {
        self.replay_blocks = Some(blocks);
        self
    }

    pub fn with_warmup_erc20_addresses(mut self, owner: Address, addresses: Vec<Address>) -> Self {
        self.warmup_erc20_addresses = Some((owner, addresses));
        self
    }

    pub fn with_token_collector(mut self, collector: TokenCollector) -> Self {
        self.token_collector = Some(collector);
        self
    }

    /// Cap concurrently executing EVM requests (call / multicall /
    /// estimateGas / simulate / trace). `0` keeps them unbounded.
    pub fn with_evm_exec_concurrency(mut self, permits: usize) -> Self {
        self.evm_exec_concurrency = permits;
        self
    }

    /// Cap concurrently executing state reads (getAddressCode /
    /// getStorageAt / nonce / balance and blockx_stateReadBatch).
    /// `0` keeps them unbounded. Deliberately a separate semaphore from
    /// the EVM limiter so disk-bound reads and CPU-bound execution
    /// cannot starve each other.
    pub fn with_state_read_concurrency(mut self, permits: usize) -> Self {
        self.state_read_concurrency = permits;
        self
    }
}

/// Bind a non-blocking TCP listener with an explicit accept-queue `backlog`.
///
/// jsonrpsee's `build(addr)` binds via tokio's `TcpListener::bind`, which uses a
/// fixed default backlog (~1024). The effective accept queue is
/// `min(backlog, net.core.somaxconn)`; when it's exceeded during a connection
/// burst the kernel drops the completing handshake (`ListenOverflows`) and the
/// client sees `dial …: i/o timeout`. Binding the socket ourselves lets the
/// backlog be raised up to `somaxconn`.
fn bind_listener(addr: &str, backlog: u32) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Socket, Type};

    let sockaddr: std::net::SocketAddr = addr.parse().map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid listen address {addr:?} (expected IP:port): {e}"),
        )
    })?;
    let socket = Socket::new(Domain::for_address(sockaddr), Type::STREAM, None)?;
    // jsonrpsee/tokio require the std listener to be non-blocking.
    socket.set_nonblocking(true)?;
    // Allow a fast rebind on restart instead of failing on a lingering socket.
    socket.set_reuse_address(true)?;
    socket.bind(&sockaddr.into())?;
    // The kernel clamps this to net.core.somaxconn.
    socket.listen(backlog.min(i32::MAX as u32) as i32)?;
    Ok(socket.into())
}

fn ensure_executor_available(cfg: &MultiChainCfgEnv) -> std::io::Result<()> {
    match cfg {
        MultiChainCfgEnv::Arc((evm_cfg, arc_cfg)) => {
            if evm_cfg.chain_id != arc_cfg.chain_id()
                || evm_cfg.spec != arc_cfg.ethereum_spec()
                || arc_cfg.chain_id() != ARC_MAINNET_CHAIN_ID
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Arc EVM configuration does not match the Arc mainnet schedule",
                ));
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Arc EVM executor is not available yet",
            ))
        }
        _ if cfg.chain_id() == ARC_MAINNET_CHAIN_ID => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "chain ID 5042 requires the Arc EVM executor",
        )),
        _ => Ok(()),
    }
}

impl<DB> ApiBuilder<DB>
where
    DB: EvmStorageRead + BlockIndex + Sync + Send + 'static,
{
    pub async fn build_and_run(
        mut self,
        addr: &str,
        max_connects: u32,
        rpc_timeout: Duration,
        is_archive: bool,
        normalize_state_key: bool,
        version: String,
        estimate_gas_buffer: u64,
        listen_backlog: u32,
    ) -> std::io::Result<ServerHandle> {
        ensure_executor_available(&self.cfg)?;

        let exec_limiter = (self.evm_exec_concurrency > 0).then(|| {
            std::sync::Arc::new(tokio::sync::Semaphore::new(self.evm_exec_concurrency))
        });
        let state_read_limiter = (self.state_read_concurrency > 0).then(|| {
            std::sync::Arc::new(tokio::sync::Semaphore::new(self.state_read_concurrency))
        });

        let http_middleware = tower::ServiceBuilder::new()
            .layer(HttpMetricLayer)
            .layer(HistoricalOverloadHttpLayer)
            .timeout(rpc_timeout);
        #[cfg(target_os = "linux")]
        let http_middleware = http_middleware.layer(InterceptorLayer::new(
            &self.interceptor_cfg.unwrap_or_default(),
        ));

        let rpc_middleware = RpcServiceBuilder::new()
            .layer_fn(|service| HistoricalOverloadRpc::new(service))
            .layer_fn(|service| RpcMetric { service });
        // Bind the listener ourselves so we can set the accept-queue backlog
        // (jsonrpsee's `build(addr)` uses tokio's default of ~1024). The kernel
        // caps the effective queue at `min(backlog, net.core.somaxconn)`, so a
        // too-small app backlog silently drops completing handshakes under
        // bursts (ListenOverflows) -> the proxy sees `dial ...: i/o timeout`.
        let listener = bind_listener(addr, listen_backlog)?;
        let server = ServerBuilder::default()
            .max_connections(max_connects)
            .http_only()
            .max_response_body_size(u32::MAX)
            .set_http_middleware(http_middleware)
            .set_rpc_middleware(rpc_middleware)
            .build_from_tcp(listener)?;
        let mut rpc_module = RpcModule::new(());
        macro_rules! run_chain_setup {
            ($cfg:expr, $custom_evm_cfg: expr) => {{
                let api_impl = ApiImpl::new(
                    self.db,
                    $cfg,
                    $custom_evm_cfg,
                    self.ovm_address.clone(),
                    self.historical_client.clone(),
                    self.historical_height,
                    is_archive,
                    normalize_state_key,
                    version.clone(),
                    estimate_gas_buffer,
                    self.token_collector.clone(),
                    exec_limiter.clone(),
                    state_read_limiter.clone(),
                );
                let api = Api::new(api_impl);
                warmup_api(
                    &api,
                    self.replay_blocks.take(),
                    self.warmup_erc20_addresses.take(),
                )
                .await;
                register_api(&mut rpc_module, api)?;
            }};
        }

        match self.cfg.clone() {
            MultiChainCfgEnv::Mainnet(env) => {
                run_chain_setup!(env, None::<NoneEvmCustomConfig>)
            }
            MultiChainCfgEnv::Arc((env, custom_evm_cfg)) => {
                run_chain_setup!(env, Some(custom_evm_cfg))
            }
            MultiChainCfgEnv::Arbitrum((env, custom_evm_cfg)) => {
                run_chain_setup!(env, custom_evm_cfg)
            }
            MultiChainCfgEnv::Op(env) => run_chain_setup!(env, None),
            MultiChainCfgEnv::Base(env) => {
                run_chain_setup!(env, None::<NoneEvmCustomConfig>)
            }
            MultiChainCfgEnv::Bsc(env) => run_chain_setup!(env, None),
            MultiChainCfgEnv::Cosmos((env, custom_evm_cfg)) => {
                run_chain_setup!(env, custom_evm_cfg)
            }
            MultiChainCfgEnv::Iotex(env) => run_chain_setup!(env, None::<NoneEvmCustomConfig>),
            MultiChainCfgEnv::Mantle(env) => run_chain_setup!(env, None),
            MultiChainCfgEnv::Moonbeam(env) => {
                run_chain_setup!(env, None::<NoneEvmCustomConfig>)
            }
            MultiChainCfgEnv::Polygon(env) => {
                run_chain_setup!(env, None::<NoneEvmCustomConfig>)
            }
            MultiChainCfgEnv::Hemi(env) => {
                run_chain_setup!(env, None::<NoneEvmCustomConfig>)
            }
            MultiChainCfgEnv::Tempo(env) => {
                // Tempo: set virtual balance placeholder (no native token).
                // Writer returns this for all eth_getBalance calls.
                let api_impl = ApiImpl::new(
                    self.db,
                    env,
                    Some(TempoEvmCustomConfig),
                    self.ovm_address.clone(),
                    self.historical_client.clone(),
                    self.historical_height,
                    is_archive,
                    normalize_state_key,
                    version.clone(),
                    estimate_gas_buffer,
                    self.token_collector.clone(),
                    exec_limiter.clone(),
                    state_read_limiter.clone(),
                );
                let api = Api::new(api_impl);
                warmup_api(
                    &api,
                    self.replay_blocks.take(),
                    self.warmup_erc20_addresses.take(),
                )
                .await;
                register_api(&mut rpc_module, api)?;
            }
            MultiChainCfgEnv::Citrea(env) => {
                run_chain_setup!(env, None::<NoneEvmCustomConfig>)
            }
        };

        let handle = server.start(rpc_module);
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_chains::arc::ArcChainConfig;
    use leafage_evm_types::{CfgEnv, MainnetSpecId};

    #[test]
    fn executor_guard_rejects_arc_chain_id_on_the_generic_mainnet_variant() {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
        cfg.chain_id = ARC_MAINNET_CHAIN_ID;

        let error = ensure_executor_available(&MultiChainCfgEnv::Mainnet(cfg)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            error.to_string(),
            "chain ID 5042 requires the Arc EVM executor"
        );
    }

    #[test]
    fn executor_guard_rejects_inconsistent_or_unavailable_arc_variants() {
        let arc_cfg = ArcChainConfig::mainnet();
        let mut evm_cfg = CfgEnv::new_with_spec(arc_cfg.ethereum_spec());
        evm_cfg.chain_id = arc_cfg.chain_id();

        let error = ensure_executor_available(&MultiChainCfgEnv::Arc((evm_cfg.clone(), arc_cfg)))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);

        evm_cfg.chain_id = 1;
        let error =
            ensure_executor_available(&MultiChainCfgEnv::Arc((evm_cfg, arc_cfg))).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn executor_guard_keeps_other_chain_variants_available() {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
        cfg.chain_id = 1;

        assert!(ensure_executor_available(&MultiChainCfgEnv::Mainnet(cfg)).is_ok());
    }
}

async fn warmup_api<A>(
    api: &Api<A>,
    blocks: Option<Vec<Vec<DebankTransaction>>>,
    erc20_addresses: Option<(Address, Vec<Address>)>,
) where
    A: ApiCore,
    A::DB: EvmStorageRead + BlockIndex,
    A::TransactionError: ToJsonRpcError + GetTransactionError,
    A::EvmHaltReason: std::fmt::Debug + Clone + GetHaltReason,
    DebankErrorCode: From<<A as EvmExecutor>::EvmHaltReason>,
{
    if let Some(blocks) = blocks {
        if let Err(err) = api.replay_blocks(blocks).await {
            error!("Error while replaying blocks: {}", err);
        }
    }
    if let Some((owner, erc20_addresses)) = erc20_addresses {
        if let Err(err) = api.warmup_erc20_address(&owner, &erc20_addresses).await {
            error!("Error while warmup erc20 address: {}", err);
        }
    }
}

fn register_api<A>(rpc_module: &mut RpcModule<()>, api: Api<A>) -> std::io::Result<()>
where
    A: ApiCore,
    <A as ApiBase>::DB: BlockIndex + EvmStorageRead,
    <A as EvmExecutor>::EvmHaltReason: GetHaltReason,
    DebankErrorCode: From<<A as EvmExecutor>::EvmHaltReason>,
    PreErrorCode: From<<A as EvmExecutor>::EvmHaltReason>,
{
    rpc_module
        .merge(DebankApiServer::into_rpc(api.clone()))
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to merge rpc module: {}", e),
            )
        })?;
    rpc_module
        .merge(PreApiServer::into_rpc(api.clone()))
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to merge rpc module: {}", e),
            )
        })?;
    rpc_module
        .merge(EthApiServer::into_rpc(api.clone()))
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to merge rpc module: {}", e),
            )
        })?;
    rpc_module
        .merge(BlockxApiServer::into_rpc(api.clone()))
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to merge rpc module: {}", e),
            )
        })?;
    Ok(())
}
