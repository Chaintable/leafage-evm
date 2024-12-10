use super::utils::{EtcdRegisterConfig, NodeInfo, NodeType, StateType};
use anyhow::{bail, Ok, Result};
use etcd_client::{Client, PutOptions};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{error, info};

pub struct Register {
    etcd_cfg: EtcdRegisterConfig,
    etcd_client: Client,
    lease_id: i64,
}

impl Register {
    pub async fn new(
        chain_id: u64,
        etcd_cfg: EtcdRegisterConfig,
        is_archive: bool,
    ) -> Result<Self> {
        let mut etcd_client = etcd_client::Client::connect(&etcd_cfg.endpoints, None).await?;
        let lease = etcd_client.lease_grant(etcd_cfg.lease_ttl_s, None).await?;
        let lease_id = lease.id();
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
        let key = format!("replicaState/{chain_id}/node/{ip}_{port}");
        let value = serde_json::to_string(&NodeInfo {
            state_type: StateType::Delay as u64,
            address: ip.to_string(),
            port,
            node_type: if is_archive {
                NodeType::Archive
            } else {
                NodeType::State
            } as u64,
        })?;
        etcd_client
            .put(
                key.clone(),
                value,
                Some(PutOptions::new().with_lease(lease_id)),
            )
            .await?;
        info!(target: "register", "register {key} success");
        Ok(Self {
            etcd_cfg,
            etcd_client,
            lease_id,
        })
    }

    pub async fn start(mut self) -> watch::Sender<()> {
        let (tx, mut rx) = watch::channel(());
        let keep_alive_interval = Duration::from_millis(self.etcd_cfg.keep_alive_interval_ms);
        let mut interval = interval(keep_alive_interval);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        break;
                    }
                    _ = interval.tick() => {
                        let res = self.etcd_client
                            .lease_keep_alive(self.lease_id)
                            .await;
                        if let Err(e) = res {
                            error!(target: "register", "keep alive error: {e}");
                            break;
                        }
                    }
                }
            }
        });
        tx
    }
}

pub async fn register_build(
    chain_id: u64,
    etcd_cfg: Option<EtcdRegisterConfig>,
    is_archive: bool,
) -> Result<watch::Sender<()>> {
    if let Some(etcd_cfg) = etcd_cfg {
        let register = Register::new(chain_id, etcd_cfg, is_archive).await?;
        let register_handle = register.start().await;
        Ok(register_handle)
    } else {
        Ok(tokio::sync::watch::channel(()).0)
    }
}
