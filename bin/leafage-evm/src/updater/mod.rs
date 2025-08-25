mod http_updater;
pub use http_updater::Updater as HttpUpdater;

mod kafka_updater;
pub use kafka_updater::Updater as KafkaUpdater;

use crate::utils::KafkaS3Config;
use anyhow::Result;
use leafage_evm_storage::{EvmStorageRead, EvmStorageWrite};
use std::time::Duration;
use tokio::sync::watch;

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
) -> Result<watch::Sender<()>> {
    match (rpc_url, kafka_s3_cfg) {
        (Some(rpc_url), None) => {
            let updater = HttpUpdater::new(tree, rpc_url, update_interval, max_diff_depth)?;
            let updater_handle = updater.start();
            Ok(updater_handle)
        }
        (rpc_url, Some(kafka_s3_cfg)) => {
            let updater = KafkaUpdater::new(
                tree,
                rpc_url,
                kafka_s3_cfg,
                max_diff_depth,
                init_task_queue_size,
            )
            .await?;
            let updater_handle = updater.start();
            Ok(updater_handle)
        }
        (None, None) => Ok(tokio::sync::watch::channel(()).0),
    }
}
