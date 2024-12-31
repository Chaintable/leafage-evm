use metrics::{Gauge, Histogram};
use metrics_derive::Metrics;
use std::sync::LazyLock;

/// The metrics for the EVM storage.
pub(crate) static STORAGE_METRICS: LazyLock<StorageMetrics> =
    LazyLock::new(|| StorageMetrics::default());

#[derive(Metrics, Clone)]
#[metrics(scope = "leafage_storage")]
pub struct StorageMetrics {
    /// Read block hash latency.
    pub read_block_hash_latency: Histogram,
    /// Read block latency.
    pub read_block_latency: Histogram,
    /// Read latest block hash latency.
    pub read_latest_block_hash_latency: Histogram,
    /// Read account latency.
    pub read_account_latency: Histogram,
    /// Read storage latency.
    pub read_storage_latency: Histogram,
    /// Read code latency.
    pub read_code_latency: Histogram,
    /// Commit block latency.
    pub commit_block_latency: Histogram,
    /// latest commit block.
    pub latest_commit_block: Gauge,
    /// latest memory block.
    pub latest_memory_block: Gauge,
    /// latest memory block timestamp
    pub latest_memory_block_timestamp: Gauge,
}
