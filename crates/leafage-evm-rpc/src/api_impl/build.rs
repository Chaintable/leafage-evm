use super::ApiImpl;
use crate::api::{DebankApiServer, EthApiServer, PreApiServer, TraceApiServer};
use crate::metrics::RpcMetric;
use jsonrpsee::server::{RpcServiceBuilder, ServerBuilder, ServerHandle};
use jsonrpsee::RpcModule;
use leafage_evm_storage::{BlockIndex, EvmStorageRead, TransactionIndex};
use revm::primitives::{CfgEnv, SpecId};
use std::sync::Arc;
use std::time::Duration;

pub struct ApiBuilder<DB> {
    db: Arc<DB>,
    cfg: CfgEnv,
    spec_id: SpecId,
}

impl<DB> ApiBuilder<DB>
where
    DB: EvmStorageRead + BlockIndex + TransactionIndex + Sync + Send + 'static,
{
    pub fn new(db: DB, cfg: CfgEnv, spec_id: SpecId) -> Self {
        Self {
            db: Arc::new(db),
            cfg,
            spec_id,
        }
    }

    pub async fn build_and_run(
        self,
        addr: &str,
        max_connects: u32,
        rpc_timeout: Duration,
    ) -> std::io::Result<ServerHandle> {
        let http_middleware = tower::ServiceBuilder::new().timeout(rpc_timeout);
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
        rpc_module
            .merge(EthApiServer::into_rpc(ApiImpl::new(
                self.db.clone(),
                self.cfg.clone(),
                self.spec_id,
            )))
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to merge rpc module: {}", e),
                )
            })?;
        rpc_module
            .merge(PreApiServer::into_rpc(ApiImpl::new(
                self.db.clone(),
                self.cfg.clone(),
                self.spec_id,
            )))
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to merge rpc module: {}", e),
                )
            })?;
        rpc_module
            .merge(TraceApiServer::into_rpc(ApiImpl::new(
                self.db.clone(),
                self.cfg.clone(),
                self.spec_id,
            )))
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to merge rpc module: {}", e),
                )
            })?;

        rpc_module
            .merge(DebankApiServer::into_rpc(ApiImpl::new(
                self.db.clone(),
                self.cfg.clone(),
                self.spec_id,
            )))
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to merge rpc module: {}", e),
                )
            })?;
        let handle = server.start(rpc_module);
        Ok(handle)
    }
}
