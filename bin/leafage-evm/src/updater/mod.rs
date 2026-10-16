mod http_updater;
pub use http_updater::Updater as HttpUpdater;

mod kafka_updater;
pub use kafka_updater::Updater as KafkaUpdater;

use crate::utils::{KafkaS3Config, StateDiffKey};
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
    state_diff_key: StateDiffKey,
    update_interval: Duration,
    max_diff_depth: usize,
    init_task_queue_size: usize,
    catchup_safe_depth: usize,
    bundle_range_size_mib: u32,
    catchup_max_lag: Duration,
) -> Result<(watch::Sender<()>, watch::Receiver<bool>)> {
    // Only the Kafka updater knows how far the node trails the chain head;
    // the other modes report caught up right away (the previous behavior).
    let caught_up = || watch::channel(true).1;
    match (rpc_url, kafka_s3_cfg) {
        (Some(rpc_url), None) => {
            let updater = HttpUpdater::new(tree, rpc_url, update_interval, max_diff_depth)?;
            Ok((updater.start(), caught_up()))
        }
        (rpc_url, Some(kafka_s3_cfg)) => {
            let updater = KafkaUpdater::new(
                tree,
                rpc_url,
                kafka_s3_cfg,
                state_diff_key,
                max_diff_depth,
                init_task_queue_size,
                catchup_safe_depth,
                bundle_range_size_mib,
                catchup_max_lag,
            )
            .await?;
            Ok(updater.start())
        }
        (None, None) => Ok((watch::channel(()).0, caught_up())),
    }
}
