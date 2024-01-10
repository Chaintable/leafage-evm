use crate::api::EthApiServer;
use crate::api_impl::EthApiImpl;
use jsonrpsee::server::{ServerBuilder, ServerHandle};
use leafage_evm_storage::EvmStorageRead;
use revm::primitives::CfgEnv;
use std::sync::Arc;
use std::time::Duration;

pub struct ApiBuilder<DB> {
    db: Arc<DB>,
    cfg: CfgEnv,
}

impl<DB> ApiBuilder<DB>
where
    DB: EvmStorageRead + Sync + Send + 'static,
{
    pub fn new(db: DB, cfg: CfgEnv) -> Self {
        Self {
            db: Arc::new(db),
            cfg,
        }
    }

    pub async fn build_and_run(
        self,
        addr: &str,
        max_connects: u32,
        rpc_timeout: Duration,
    ) -> std::io::Result<ServerHandle> {
        let service_builder = tower::ServiceBuilder::new().timeout(rpc_timeout);
        let server = ServerBuilder::default()
            .max_connections(max_connects)
            .http_only()
            .max_response_body_size(u32::MAX)
            .set_http_middleware(service_builder)
            .build(addr)
            .await?;
        let handle = server.start(EthApiImpl::new(self.db.clone(), self.cfg.clone()).into_rpc());
        Ok(handle)
    }
}
