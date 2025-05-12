use super::utils::{EtcdRegisterConfig, NodeInfo, NodeType, StateType};
use anyhow::{bail, Ok, Result};
use etcd_client::{Client, LeaseKeepAliveStream, LeaseKeeper, PutOptions};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{error, info};

pub struct Register {
    etcd_cfg: EtcdRegisterConfig,
    etcd_client: Client,
    lease_id: i64,
    key: String,
    value: String,
}

impl Register {
    async fn register(&mut self) -> Result<(LeaseKeeper, LeaseKeepAliveStream)> {
        let lease = self
            .etcd_client
            .lease_grant(self.etcd_cfg.lease_ttl_s, None)
            .await?;
        let lease_id = lease.id();
        self.lease_id = lease_id;
        self.etcd_client
            .put(
                self.key.clone(),
                self.value.clone(),
                Some(PutOptions::new().with_lease(lease_id)),
            )
            .await?;
        info!(target: "register", "register key:{}, lease_id: {} success",self.key,self.lease_id);
        let (keeper, stream) = self.etcd_client.lease_keep_alive(self.lease_id).await?;
        Ok((keeper, stream))
    }

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
        let key = format!("{chain_id}/nodes/{ip}_{port}");
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

        Ok(Self {
            etcd_cfg,
            etcd_client,
            lease_id,
            key,
            value,
        })
    }

    pub async fn start(mut self) -> Result<watch::Sender<()>> {
        let (tx, mut rx) = watch::channel(());
        let keep_alive_interval = Duration::from_millis(self.etcd_cfg.keep_alive_interval_ms);
        let mut interval = interval(keep_alive_interval);
        let (mut keeper, mut stream) = self.register().await?;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        break;
                    }
                    resp = stream.message() => {
                        match resp {
                            Result::Ok(Some(rsp)) => {
                                if rsp.ttl() <= 0 {
                                    error!(target: "register", "lease {:?} ttl <= 0", self.lease_id);
                                    let rsp = self.register().await;
                                    if rsp.is_err() {
                                        continue;
                                    }
                                    (keeper, stream) = rsp.unwrap();
                                }
                            }
                            Result::Ok(None) => {
                                error!(target: "register", "lease {:?} is none", self.lease_id);
                                let rsp = self.register().await;
                                if rsp.is_err() {
                                    continue;
                                }
                                (keeper, stream) = rsp.unwrap();
                            }
                            Result::Err(e) => {
                                error!(target: "register", "lease {:?} keep alive error: {e}", self.lease_id);
                            }
                        }
                    }
                    _ = interval.tick() => {
                        let res = keeper.keep_alive().await;
                        if let Err(_) = res {
                            let rsp = self.register().await;
                            if rsp.is_err() {
                                continue;
                            }
                            (keeper, stream) = rsp.unwrap();
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
    etcd_cfg: Option<EtcdRegisterConfig>,
    is_archive: bool,
) -> Result<watch::Sender<()>> {
    if let Some(etcd_cfg) = etcd_cfg {
        let register = Register::new(chain_id, etcd_cfg, is_archive).await?;
        let register_handle = register.start().await?;
        Ok(register_handle)
    } else {
        Ok(tokio::sync::watch::channel(()).0)
    }
}
