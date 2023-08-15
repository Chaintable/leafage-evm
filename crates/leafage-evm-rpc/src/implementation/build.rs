use crate::api::EthApiServer;
use crate::implementation::EthApiImpl;
use jsonrpsee::{
    core::Error,
    server::{ServerBuilder, ServerHandle},
    RpcModule,
};
use leafage_evm_storage::EvmStorageRead;
use revm::primitives::CfgEnv;
use std::sync::Arc;

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

    fn build_rpc(&self) -> RpcModule<EthApiImpl<Arc<DB>>> {
        let rpc_module = EthApiImpl::new(self.db.clone(), self.cfg.clone()).into_rpc();
        rpc_module
    }

    pub async fn build_and_run(self, addr: &str) -> Result<ServerHandle, Error> {
        let server = ServerBuilder::default().build(addr).await?;
        let handle = server.start(self.build_rpc())?;
        Ok(handle)
    }
}
