mod arc_guard;

mod http_updater;
pub use http_updater::Updater as HttpUpdater;

mod kafka_updater;
pub use kafka_updater::Updater as KafkaUpdater;

use crate::utils::KafkaS3Config;
use anyhow::{bail, Error, Result};
use leafage_evm_storage::{EvmStorageRead, EvmStorageWrite};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::{oneshot, watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdaterInputPolicy {
    Legacy,
    ArcFinalizedLinear,
}

impl UpdaterInputPolicy {
    pub(crate) fn is_arc(self) -> bool {
        matches!(self, Self::ArcFinalizedLinear)
    }
}

pub(crate) struct UpdaterHandle {
    pub(crate) stop: watch::Sender<()>,
    pub(crate) fatal: Option<oneshot::Receiver<Error>>,
}

pub(crate) struct ArcFatalReporter {
    fatal: Arc<AtomicBool>,
    sender: Option<oneshot::Sender<Error>>,
}

impl ArcFatalReporter {
    fn new(fatal: Arc<AtomicBool>, sender: oneshot::Sender<Error>) -> Self {
        Self {
            fatal,
            sender: Some(sender),
        }
    }

    pub(crate) fn report(&mut self, error: Error) {
        self.fatal.store(true, Ordering::SeqCst);
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(error);
        }
    }
}

pub(crate) async fn updater_build<
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
    catchup_safe_depth: usize,
    bundle_range_size_mib: u32,
    input_policy: UpdaterInputPolicy,
    arc_fatal: Arc<AtomicBool>,
) -> Result<UpdaterHandle> {
    match (rpc_url, kafka_s3_cfg) {
        (Some(rpc_url), None) => {
            if input_policy.is_arc() {
                bail!("Arc finalized input requires Kafka/S3; HTTP-only updater is unsupported");
            }
            let updater = HttpUpdater::new(tree, rpc_url, update_interval, max_diff_depth)?;
            Ok(UpdaterHandle {
                stop: updater.start(),
                fatal: None,
            })
        }
        (rpc_url, Some(kafka_s3_cfg)) => {
            let updater = KafkaUpdater::new(
                tree,
                rpc_url,
                kafka_s3_cfg,
                max_diff_depth,
                init_task_queue_size,
                catchup_safe_depth,
                bundle_range_size_mib,
                input_policy,
            )
            .await?;
            if input_policy.is_arc() {
                let (sender, receiver) = oneshot::channel();
                Ok(UpdaterHandle {
                    stop: updater.start(Some(ArcFatalReporter::new(arc_fatal, sender))),
                    fatal: Some(receiver),
                })
            } else {
                Ok(UpdaterHandle {
                    stop: updater.start(None),
                    fatal: None,
                })
            }
        }
        (None, None) if input_policy.is_arc() => {
            bail!("Arc finalized input requires Kafka/S3 updater configuration")
        }
        (None, None) => Ok(UpdaterHandle {
            stop: tokio::sync::watch::channel(()).0,
            fatal: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn arc_fatal_is_sticky_when_startup_becomes_ready_later() {
        let startup_ready = AtomicBool::new(false);
        let fatal = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = oneshot::channel();
        let mut reporter = ArcFatalReporter::new(fatal.clone(), sender);

        reporter.report(anyhow::anyhow!("bad Arc input"));
        startup_ready.store(true, Ordering::SeqCst);

        assert!(startup_ready.load(Ordering::SeqCst));
        assert!(fatal.load(Ordering::SeqCst));
        assert!(!(startup_ready.load(Ordering::SeqCst) && !fatal.load(Ordering::SeqCst)));
        assert_eq!(receiver.await.unwrap().to_string(), "bad Arc input");
    }

    #[test]
    fn legacy_policy_never_uses_arc_guard() {
        assert!(!UpdaterInputPolicy::Legacy.is_arc());
        assert!(UpdaterInputPolicy::ArcFinalizedLinear.is_arc());
    }
}
