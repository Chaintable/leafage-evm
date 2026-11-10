use metrics::{Counter, Gauge, Histogram};
use metrics_derive::Metrics;
use std::sync::LazyLock;

/// The metrics for the EVM storage.
pub(crate) static STORAGE_METRICS: LazyLock<StorageMetrics> =
    LazyLock::new(|| StorageMetrics::default());

/// RocksDB statistics of the archive database, copied from its tickers.
pub(crate) static ROCKSDB_METRICS: LazyLock<RocksdbMetrics> =
    LazyLock::new(RocksdbMetrics::default);

/// The metrics for the latest block
pub(crate) static BLOCK_METRICS: LazyLock<BlockMetrics> = LazyLock::new(|| BlockMetrics::default());

#[derive(Metrics, Clone)]
#[metrics(scope = "leafage_storage")]
pub struct StorageMetrics {
    /// Read block hash latency.
    pub read_block_hash_latency: Histogram,
    /// Read block latency (`BlockHashToBlockInfo`, by hash).
    pub read_block_latency: Histogram,
    /// Read block latency from `BlockNumToBlockInfo` (by number, found
    /// through the archive block index).
    pub read_block_by_number_latency: Histogram,
    /// Read latest block hash latency.
    pub read_latest_block_hash_latency: Histogram,
    /// Read account latency.
    pub read_account_latency: Histogram,
    /// Read storage latency.
    pub read_storage_latency: Histogram,
    /// Read code latency (disk reads only; code-cache hits are not recorded).
    pub read_code_latency: Histogram,
    /// Batched account read latency (one sample per MultiGet call).
    pub read_account_many_latency: Histogram,
    /// Batched storage read latency (one sample per MultiGet call).
    pub read_storage_many_latency: Histogram,
    /// Batched code read latency (one sample per MultiGet call).
    pub read_code_many_latency: Histogram,
    /// Archive code-cache hits (cumulative).
    pub code_cache_hits: Counter,
    /// Archive code-cache misses (cumulative).
    pub code_cache_misses: Counter,
    /// Archive state-cache account hits (cumulative).
    pub account_cache_hits: Counter,
    /// Archive state-cache account misses (cumulative).
    pub account_cache_misses: Counter,
    /// Archive state-cache storage hits (cumulative).
    pub storage_cache_hits: Counter,
    /// Archive state-cache storage misses (cumulative).
    pub storage_cache_misses: Counter,
    /// Archive state-cache misses that probed for the key's next version.
    pub state_cache_probes: Counter,
    /// Probes that found a version above the one read, bounding its span.
    pub state_cache_probes_newer: Counter,
    /// Exact schema v3 boundary-seek latency, including unsuccessful probes.
    pub state_cache_interval_probe_latency: Histogram,
    /// Exact probes that failed; the successful read's original span is kept.
    pub state_cache_interval_probe_errors: Counter,
    /// Misses that skipped the probe: the read was at the head.
    pub state_cache_probes_skipped_at_head: Counter,
    /// Misses that skipped the probe: a newer version is known.
    pub state_cache_probes_skipped_capped: Counter,
    /// Archive state-cache misses on a key not in the cache.
    pub state_cache_cold_misses: Counter,
    /// Archive block-info cache hits (cumulative).
    pub block_info_cache_hits: Counter,
    /// Schema v3 archive account Gets that found a version at or below the
    /// read height.
    pub archive_account_gets_found: Counter,
    /// Schema v3 archive account Gets that found no version at or below the
    /// read height.
    pub archive_account_gets_missing: Counter,
    /// Schema v3 archive storage Gets that found a version at or below the
    /// read height.
    pub archive_storage_gets_found: Counter,
    /// Schema v3 archive storage Gets that found no version at or below the
    /// read height.
    pub archive_storage_gets_missing: Counter,
    /// Commit block latency.
    pub commit_block_latency: Histogram,
    /// latest commit block.
    pub latest_commit_block: Gauge,
    /// Active iterators in Archive mode.
    pub active_iterators: Gauge,
    /// Timed out iterators in Archive mode.
    pub timed_out_iterators: Gauge,
    /// Force released iterators in Archive mode (cumulative counter).
    pub force_released_iterators: Gauge,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "pipeline")]
pub struct BlockMetrics {
    /// latest memory block.
    pub block_num: Gauge,
    /// latest memory block timestamp
    pub block_time: Gauge,
}

/// Cumulative RocksDB tickers of the archive database (statistics level
/// `ExceptHistogramOrTimers`), plus its block cache usage.
#[derive(Metrics, Clone)]
#[metrics(scope = "leafage_rocksdb")]
pub struct RocksdbMetrics {
    /// Point lookups (Get/MultiGet keys).
    pub keys_read: Counter,
    /// Block cache hits on index blocks.
    pub block_cache_index_hit: Counter,
    /// Block cache misses on index blocks.
    pub block_cache_index_miss: Counter,
    /// Block cache hits on filter blocks.
    pub block_cache_filter_hit: Counter,
    /// Block cache misses on filter blocks.
    pub block_cache_filter_miss: Counter,
    /// Block cache hits on data blocks.
    pub block_cache_data_hit: Counter,
    /// Block cache misses on data blocks.
    pub block_cache_data_miss: Counter,
    /// Point lookups a filter ruled out, saving the file read.
    pub bloom_filter_useful: Counter,
    /// Point lookups a filter let through.
    pub bloom_filter_full_positive: Counter,
    /// Point lookups a filter let through and the key was in the file.
    pub bloom_filter_full_true_positive: Counter,
    /// Point lookups served from L0.
    pub get_hit_l0: Counter,
    /// Point lookups served from L1.
    pub get_hit_l1: Counter,
    /// Point lookups served from L2 and below.
    pub get_hit_l2_and_up: Counter,
    /// File reads (page cache or disk) from the last level.
    pub last_level_read_count: Counter,
    /// Bytes read from last-level files.
    pub last_level_read_bytes: Counter,
    /// File reads (page cache or disk) from the other levels.
    pub non_last_level_read_count: Counter,
    /// Bytes read from other-level files.
    pub non_last_level_read_bytes: Counter,
    /// Block cache capacity in bytes.
    pub block_cache_capacity_bytes: Gauge,
    /// Bytes held in the block cache.
    pub block_cache_usage_bytes: Gauge,
    /// Bytes pinned in the block cache.
    pub block_cache_pinned_usage_bytes: Gauge,
}
