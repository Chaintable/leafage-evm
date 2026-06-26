use super::utils::{EtcdRegisterConfig, NodeInfo, NodeType, StateType};
use anyhow::{bail, Ok, Result};
use etcd_client::{Client, Compare, CompareOp, Txn, TxnOp};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{debug, error, info};

pub struct Register {
    etcd_cfg: EtcdRegisterConfig,
    etcd_client: Client,
    key: String,
    value: String,
}

impl Register {
    async fn register(&mut self) -> Result<()> {
        self.etcd_client
            .put(self.key.clone(), self.value.clone(), None)
            .await?;
        info!(target: "register", "register key:{}, success",self.key);
        Ok(())
    }

    async fn unregister(&mut self) -> Result<()> {
        self.etcd_client.delete(self.key.clone(), None).await?;
        info!(target: "register", "unregister key:{} success", self.key);
        Ok(())
    }

    pub async fn new(
        chain_id: u64,
        version: String,
        etcd_cfg: EtcdRegisterConfig,
        node_type: NodeType,
    ) -> Result<Self> {
        let etcd_client = etcd_client::Client::connect(&etcd_cfg.endpoints, None).await?;
        let meta = etcd_cfg.meta.clone();
        if meta.is_empty() {
            bail!("meta is empty");
        }
        let ip_host = meta.split(":").collect::<Vec<&str>>();
        if ip_host.len() != 2 {
            bail!("meta format error");
        }
        let ip = ip_host[0];
        let port = ip_host[1].parse::<u64>()?;
        let key = if version.is_empty() {
            format!("{chain_id}/nodes/{ip}_{port}")
        } else {
            format!("{chain_id}/{version}/nodes/{ip}_{port}")
        };
        let value = serde_json::to_string(&NodeInfo {
            state_type: StateType::Delay as u64,
            address: ip.to_string(),
            port,
            node_type: node_type as u64,
        })?;

        Ok(Self {
            etcd_cfg,
            etcd_client,
            key,
            value,
        })
    }

    pub async fn start(mut self) -> Result<watch::Sender<()>> {
        let (tx, mut rx) = watch::channel(());
        let keep_alive_interval = Duration::from_millis(self.etcd_cfg.keep_alive_interval_ms);
        let mut interval = interval(keep_alive_interval);
        self.register().await?;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        let err = self.unregister().await;
                        if let Err(e) = err {
                            error!(target: "register", "unregister error: {e}");
                        }
                        break;
                    }
                    _ = interval.tick() => {
                        // 使用事务仅在key不存在时注册（version=0表示key不存在）
                        let txn = Txn::new()
                            .when(vec![Compare::version(self.key.clone(), CompareOp::Equal, 0)])
                            .and_then(vec![TxnOp::put(self.key.clone(), self.value.clone(), None)])
                            .or_else(vec![]);

                        match self.etcd_client.txn(txn).await {
                            Result::Ok(resp) => {
                                if resp.succeeded() {
                                    info!(target: "register", "register key:{}, success", self.key);
                                } else {
                                    debug!(target: "register", "key:{} already exists, skip registration", self.key);
                                }
                            }
                            Result::Err(e) => {
                                error!(target: "register", "register error: {e}");
                            }
                        }
                    }
                }
            }
        });
        Ok(tx)
    }
}

pub async fn register_build(
    chain_id: u64,
    version: String,
    etcd_cfg: Option<EtcdRegisterConfig>,
    node_type: NodeType,
) -> Result<watch::Sender<()>> {
    if let Some(etcd_cfg) = etcd_cfg {
        let register = Register::new(chain_id, version, etcd_cfg, node_type).await?;
        let register_handle = register.start().await?;
        Ok(register_handle)
    } else {
        Ok(tokio::sync::watch::channel(()).0)
    }
}
