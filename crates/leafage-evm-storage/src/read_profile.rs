//! Thread-local RocksDB counters for bounded diagnostic scopes. File-system
//! reads include OS page-cache hits; these are not physical device I/O counts.

use rocksdb::perf::{set_perf_stats, PerfContext, PerfMetric, PerfStatsLevel};
use std::{cell::Cell, collections::BTreeMap};

thread_local! {
    static PROFILING: Cell<bool> = const { Cell::new(false) };
}

/// Profiles one synchronous operation on the calling thread. Nested scopes
/// leave the outer counters alone. Always disable timing again, including on
/// unwind, before the blocking-pool thread can service another request.
pub fn profile_rocksdb_reads<T>(read: impl FnOnce() -> T) -> (T, BTreeMap<&'static str, u64>) {
    if PROFILING.replace(true) {
        return (read(), BTreeMap::new());
    }
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            set_perf_stats(PerfStatsLevel::Disable);
            PROFILING.set(false);
        }
    }
    let _guard = Guard;
    let mut context = PerfContext::default();
    context.reset();
    set_perf_stats(PerfStatsLevel::EnableTime);
    let result = read();
    let counters = [
        ("block_cache_hits", PerfMetric::BlockCacheHitCount),
        ("block_reads", PerfMetric::BlockReadCount),
        ("block_read_bytes", PerfMetric::BlockReadByte),
        ("block_read_ns", PerfMetric::BlockReadTime),
        ("decompress_ns", PerfMetric::BlockDecompressTime),
        ("memtable_ns", PerfMetric::GetFromMemtableTime),
        ("output_files_ns", PerfMetric::GetFromOutputFilesTime),
        ("index_block_ns", PerfMetric::ReadIndexBlockNanos),
        ("filter_block_ns", PerfMetric::ReadFilterBlockNanos),
        ("table_lookup_ns", PerfMetric::FindTableNanos),
        ("db_mutex_ns", PerfMetric::DbMutexLockNanos),
        ("db_condition_wait_ns", PerfMetric::DbConditionWaitNanos),
        ("bloom_sst_hits", PerfMetric::BloomSstHitCount),
        ("bloom_sst_misses", PerfMetric::BloomSstMissCount),
        (
            "new_random_access_file_ns",
            PerfMetric::EnvNewRandomAccessFileNanos,
        ),
    ]
    .into_iter()
    .map(|(name, metric)| (name, context.metric(metric)))
    .collect();
    (result, counters)
}
