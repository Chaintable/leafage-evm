//! Exports the archive database's RocksDB statistics as metrics.

use std::time::Duration;

use rocksdb::statistics::{StatsLevel, Ticker};
use rocksdb::{Cache, Options};

use crate::metrics::ROCKSDB_METRICS;

const EXPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Collect tickers only: histograms and timers would add clock reads to every
/// lookup.
pub(super) fn enable(opts: &mut Options) {
    opts.enable_statistics();
    opts.set_statistics_level(StatsLevel::ExceptHistogramOrTimers);
}

/// Copy the tickers and the block cache usage into the metrics every
/// [`EXPORT_INTERVAL`], for the life of the process. `opts` must be the
/// options the database was opened with, after [`enable`]; they share its
/// statistics object.
pub(super) fn spawn_exporter(opts: Options, cache: Cache, capacity: usize) {
    std::thread::Builder::new()
        .name("rocksdb-stats".into())
        .spawn(move || {
            let m = &*ROCKSDB_METRICS;
            m.block_cache_capacity_bytes.set(capacity as f64);
            let tickers = [
                (Ticker::NumberKeysRead, &m.keys_read),
                (Ticker::BlockCacheIndexHit, &m.block_cache_index_hit),
                (Ticker::BlockCacheIndexMiss, &m.block_cache_index_miss),
                (Ticker::BlockCacheFilterHit, &m.block_cache_filter_hit),
                (Ticker::BlockCacheFilterMiss, &m.block_cache_filter_miss),
                (Ticker::BlockCacheDataHit, &m.block_cache_data_hit),
                (Ticker::BlockCacheDataMiss, &m.block_cache_data_miss),
                (Ticker::BloomFilterUseful, &m.bloom_filter_useful),
                (
                    Ticker::BloomFilterFullPositive,
                    &m.bloom_filter_full_positive,
                ),
                (
                    Ticker::BloomFilterFullTruePositive,
                    &m.bloom_filter_full_true_positive,
                ),
                (Ticker::GetHitL0, &m.get_hit_l0),
                (Ticker::GetHitL1, &m.get_hit_l1),
                (Ticker::GetHitL2AndUp, &m.get_hit_l2_and_up),
                (Ticker::LastLevelReadCount, &m.last_level_read_count),
                (Ticker::LastLevelReadBytes, &m.last_level_read_bytes),
                (Ticker::NonLastLevelReadCount, &m.non_last_level_read_count),
                (Ticker::NonLastLevelReadBytes, &m.non_last_level_read_bytes),
            ];
            loop {
                for (ticker, counter) in &tickers {
                    counter.absolute(opts.get_ticker_count(*ticker));
                }
                m.block_cache_usage_bytes.set(cache.get_usage() as f64);
                m.block_cache_pinned_usage_bytes
                    .set(cache.get_pinned_usage() as f64);
                std::thread::sleep(EXPORT_INTERVAL);
            }
        })
        .expect("spawn rocksdb-stats thread");
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocksdb::{BlockBasedOptions, DB};

    /// The options keep reading the open database's tickers, which
    /// [`spawn_exporter`] relies on.
    #[test]
    fn options_read_the_open_databases_tickers() {
        let dir =
            std::env::temp_dir().join(format!("leafage-rocksdb-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Cache::new_hyper_clock_cache(8 << 20, 0);
        let mut block_opts = BlockBasedOptions::default();
        block_opts.set_block_cache(&cache);
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_block_based_table_factory(&block_opts);
        enable(&mut opts);
        let db = DB::open(&opts, &dir).unwrap();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
        for _ in 0..3 {
            assert_eq!(db.get(b"k").unwrap().as_deref(), Some(b"v".as_slice()));
        }
        assert_eq!(db.get(b"absent").unwrap(), None);

        assert_eq!(opts.get_ticker_count(Ticker::NumberKeysRead), 4);
        let data = opts.get_ticker_count(Ticker::BlockCacheDataHit)
            + opts.get_ticker_count(Ticker::BlockCacheDataMiss);
        assert!(data > 0);
        assert!(cache.get_usage() > 0);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
