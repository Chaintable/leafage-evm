mod http_updater;
pub use http_updater::Updater as HttpUpdater;

mod kafka_updater;
pub use kafka_updater::Updater as KafkaUpdater;

mod ws_updater;
pub use ws_updater::Updater as WsUpdater;

use crate::utils::{GatewayObjectConfig, KafkaS3Config};
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
    ws_url: Option<String>,
    kafka_s3_cfg: Option<KafkaS3Config>,
    gateway_object_cfg: Option<GatewayObjectConfig>,
    update_interval: Duration,
    max_diff_depth: usize,
    init_task_queue_size: usize,
) -> Result<watch::Sender<()>> {
    // ws_url replaces kafka as the block-notification source; it still needs
    // KafkaS3Config for S3 (block_info / block_diff) access. When ws_url is
    // set without a KafkaS3Config it is silently ignored and the old
    // rpc/kafka/no-op logic applies.
    match (ws_url, rpc_url, kafka_s3_cfg, gateway_object_cfg) {
        (Some(ws_url), rpc_url, Some(kafka_s3_cfg), None) => {
            let updater = WsUpdater::new(
                tree,
                ws_url,
                rpc_url,
                Some(kafka_s3_cfg),
                None,
                max_diff_depth,
                init_task_queue_size,
            )
            .await?;
            Ok(updater.start())
        }
        (Some(ws_url), rpc_url, None, Some(gateway_object_cfg)) => {
            let updater = WsUpdater::new(
                tree,
                ws_url,
                rpc_url,
                None,
                Some(gateway_object_cfg),
                max_diff_depth,
                init_task_queue_size,
            )
            .await?;
            Ok(updater.start())
        }
        (Some(_), _, Some(_), Some(_)) => Err(anyhow::anyhow!("kafka_s3_config and gateway_object_config are mutually exclusive in ws mode")),
        (_, Some(rpc_url), None, None) => {
            let updater = HttpUpdater::new(tree, rpc_url, update_interval, max_diff_depth)?;
            Ok(updater.start())
        }
        (_, rpc_url, Some(kafka_s3_cfg), None) => {
            let updater = KafkaUpdater::new(
                tree,
                rpc_url,
                kafka_s3_cfg,
                max_diff_depth,
                init_task_queue_size,
            )
            .await?;
            Ok(updater.start())
        }
        (_, _, None, Some(_)) => Err(anyhow::anyhow!("gateway_object_config requires ws_url")),
        (None, _, Some(_), Some(_)) => Err(anyhow::anyhow!("kafka_s3_config and gateway_object_config are mutually exclusive")),
        (Some(_), _, None, None) => Err(anyhow::anyhow!("ws_url requires either kafka_s3_config or gateway_object_config")),
        (_, None, None, None) => Ok(tokio::sync::watch::channel(()).0),
    }
}
