//! Thread-local RocksDB diagnostics. File reads can hit the OS page cache;
//! they are not physical device I/O counts. Timers below can overlap.

use rocksdb::perf::{set_perf_stats, PerfContext, PerfMetric, PerfStatsLevel};
use std::{cell::Cell, collections::BTreeMap, marker::PhantomData, rc::Rc};

thread_local! {
    static PROFILING: Cell<bool> = const { Cell::new(false) };
}

/// A synchronous, thread-bound scope. Nested scopes do not reset the parent's
/// counters. Drop disables profiling even when the operation unwinds.
pub struct RocksDbReadProfile {
    context: PerfContext,
    _thread_bound: PhantomData<Rc<()>>,
}

impl RocksDbReadProfile {
    pub fn start() -> Option<Self> {
        if PROFILING.replace(true) {
            return None;
        }
        let mut context = PerfContext::default();
        context.reset();
        set_perf_stats(PerfStatsLevel::EnableTime);
        Some(Self {
            context,
            _thread_bound: PhantomData,
        })
    }

    pub fn snapshot(&self) -> BTreeMap<&'static str, u64> {
        [
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
        .map(|(name, metric)| (name, self.context.metric(metric)))
        .collect()
    }
}

impl Drop for RocksDbReadProfile {
    fn drop(&mut self) {
        set_perf_stats(PerfStatsLevel::Disable);
        PROFILING.set(false);
    }
}
