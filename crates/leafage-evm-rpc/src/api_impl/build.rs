#[cfg(target_os = "linux")]
use super::{ApiImpl, InterceptorConfig, InterceptorLayer};
use crate::api::{DebankApiServer, EthApiServer, PreApiServer};
use crate::api_impl::core::{Api, MultiChainCfgEnv};
use crate::metrics::RpcMetric;
use jsonrpsee::server::{RpcServiceBuilder, ServerBuilder, ServerHandle};
use jsonrpsee::{
    http_client::{HttpClient, HttpClientBuilder},
    RpcModule,
};
use leafage_evm_storage::{BlockIndex, EvmStorageRead};
use leafage_evm_types::Address;
use std::time::Duration;

pub struct ApiBuilder<DB> {
    db: DB,
    cfg: MultiChainCfgEnv,
    historical_client: Option<HttpClient>,
    historical_height: Option<u64>,
}

impl<DB> ApiBuilder<DB>
where
    DB: EvmStorageRead + BlockIndex + Sync + Send + 'static,
{
    pub fn new(db: DB, cfg: MultiChainCfgEnv) -> Self {
        Self {
            db,
            cfg,
            historical_client: None,
            historical_height: None,
        }
    }

    pub fn with_historical_config(
        mut self,
        historical_rpc: Option<String>,
        historical_height: Option<u64>,
    ) -> Self {
        if let Some(url) = historical_rpc {
            if let Ok(http_client) = HttpClientBuilder::default()
                .request_timeout(Duration::from_secs(30))
                .build(&url)
            {
                self.historical_client = Some(http_client);
            }
        }
        self.historical_height = historical_height;
        self
    }
}

impl<DB> ApiBuilder<DB>
where
    DB: EvmStorageRead + BlockIndex + Sync + Send + 'static,
{
    pub async fn build_and_run(
        self,
        addr: &str,
        max_connects: u32,
        rpc_timeout: Duration,
        #[cfg(target_os = "linux")] interceptor_cfg: Option<InterceptorConfig>,
        ovm_address: Option<Address>,
        is_archive: bool,
        normalize_state_key: bool,
    ) -> std::io::Result<ServerHandle> {
        let http_middleware = tower::ServiceBuilder::new().timeout(rpc_timeout);
        #[cfg(target_os = "linux")]
        let http_middleware =
            http_middleware.layer(InterceptorLayer::new(&interceptor_cfg.unwrap_or_default()));

        let rpc_middleware = RpcServiceBuilder::new().layer_fn(|service| RpcMetric { service });
        let server = ServerBuilder::default()
            .max_connections(max_connects)
            .http_only()
            .max_response_body_size(u32::MAX)
            .set_http_middleware(http_middleware)
            .set_rpc_middleware(rpc_middleware)
            .build(addr)
            .await?;
        let mut rpc_module = RpcModule::new(());

        match &self.cfg {
            MultiChainCfgEnv::Mainnet(cfg) => {
                let api_impl = ApiImpl::new(
                    self.db,
                    cfg.clone(),
                    rpc_timeout,
                    ovm_address.clone(),
                    self.historical_client.clone(),
                    self.historical_height,
                    is_archive,
                    normalize_state_key,
                );
                let api = Api::new(api_impl);
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
            }
            MultiChainCfgEnv::Op(cfg) => {
                let api_impl = ApiImpl::new(
                    self.db,
                    cfg.clone(),
                    rpc_timeout,
                    ovm_address.clone(),
                    self.historical_client.clone(),
                    self.historical_height,
                    is_archive,
                    normalize_state_key,
                );
                let api = Api::new(api_impl);
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
            }
        }
        let handle = server.start(rpc_module);
        Ok(handle)
    }
}
