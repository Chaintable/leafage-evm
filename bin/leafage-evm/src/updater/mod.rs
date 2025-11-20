mod http_updater;
pub use http_updater::Updater as HttpUpdater;

mod kafka_updater;
pub use kafka_updater::Updater as KafkaUpdater;

use crate::utils::KafkaS3Config;
use anyhow::Result;
use leafage_evm_storage::{EvmStorageRead, EvmStorageWrite};
use leafage_evm_types::DebankTransaction;
use std::time::Duration;
use revm::primitives::Address;
use tokio::sync::watch;

pub enum Updater<Tree> {
    Http(HttpUpdater<Tree>),
    Kafka(KafkaUpdater<Tree>),
    None,
}

impl<Tree> Updater<Tree>
where
    Tree: EvmStorageRead
        + EvmStorageWrite<Error = <Tree as EvmStorageRead>::Error>
        + Send
        + Sync
        + 'static,
{
    pub fn start(self) -> watch::Sender<()> {
        match self {
            Updater::Http(updater) => updater.start(),
            Updater::Kafka(updater) => updater.start(),
            Updater::None => tokio::sync::watch::channel(()).0,
        }
    }

    pub async fn fetch_warmup_blocks(&mut self) -> Result<Vec<Vec<DebankTransaction>>> {
        match self {
            Updater::Http(_) | Updater::None => Ok(Default::default()),
            Updater::Kafka(updater) => updater.fetch_warmup_blocks().await,
        }
    }

    pub async fn fetch_tokens(&mut self) -> Result<(Address, Vec<Address>)> {
        match self {
            Updater::Http(_) | Updater::None => Ok(Default::default()),
            Updater::Kafka(updater) => updater.fetch_tokens().await,
        }
    }
}

pub async fn updater_build<
    Tree: EvmStorageRead
        + EvmStorageWrite<Error = <Tree as EvmStorageRead>::Error>
        + Send
        + Sync
        + 'static,
>(
    tree: Tree,
    rpc_url: Option<String>,
    kafka_s3_cfg: Option<KafkaS3Config>,
    update_interval: Duration,
    max_diff_depth: usize,
    init_task_queue_size: usize,
    warmup_blocks: usize,
) -> Result<Updater<Tree>> {
    match (rpc_url, kafka_s3_cfg) {
        (Some(rpc_url), None) => {
            HttpUpdater::new(tree, rpc_url, update_interval, max_diff_depth).map(Updater::Http)
        }
        (rpc_url, Some(kafka_s3_cfg)) => KafkaUpdater::new(
            tree,
            rpc_url,
            kafka_s3_cfg,
            max_diff_depth,
            init_task_queue_size,
            warmup_blocks,
        )
        .await
        .map(Updater::Kafka),
        (None, None) => Ok(Updater::None),
    }
}
