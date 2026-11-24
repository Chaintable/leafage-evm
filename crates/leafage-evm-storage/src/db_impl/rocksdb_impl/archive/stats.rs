//! Exports the archive database's RocksDB statistics as metrics.

use std::collections::HashMap;
use std::time::Duration;

use metrics::Counter;
use rocksdb::statistics::StatsLevel;
use rocksdb::{Cache, Options};

use crate::metrics::{RocksdbMetrics, ROCKSDB_METRICS};

const EXPORT_INTERVAL: Duration = Duration::from_secs(10);

/// Collect tickers only: histograms and timers would add clock reads to every
/// lookup.
pub(super) fn enable(opts: &mut Options) {
    opts.enable_statistics();
    opts.set_statistics_level(StatsLevel::ExceptHistogramOrTimers);
}

/// The exported tickers, by RocksDB name. They are read by name from the
/// statistics dump: rust-rocksdb's `Ticker` numbering has drifted from
/// RocksDB 10.7.5's (`rocksdb.last.level.read.count` is off by 4).
fn tickers(m: &RocksdbMetrics) -> [(&'static str, &Counter); 17] {
    [
        ("rocksdb.number.keys.read", &m.keys_read),
        ("rocksdb.block.cache.index.hit", &m.block_cache_index_hit),
        ("rocksdb.block.cache.index.miss", &m.block_cache_index_miss),
        ("rocksdb.block.cache.filter.hit", &m.block_cache_filter_hit),
        (
            "rocksdb.block.cache.filter.miss",
            &m.block_cache_filter_miss,
        ),
        ("rocksdb.block.cache.data.hit", &m.block_cache_data_hit),
        ("rocksdb.block.cache.data.miss", &m.block_cache_data_miss),
        ("rocksdb.bloom.filter.useful", &m.bloom_filter_useful),
        (
            "rocksdb.bloom.filter.full.positive",
            &m.bloom_filter_full_positive,
        ),
        (
            "rocksdb.bloom.filter.full.true.positive",
            &m.bloom_filter_full_true_positive,
        ),
        ("rocksdb.l0.hit", &m.get_hit_l0),
        ("rocksdb.l1.hit", &m.get_hit_l1),
        ("rocksdb.l2andup.hit", &m.get_hit_l2_and_up),
        ("rocksdb.last.level.read.count", &m.last_level_read_count),
        ("rocksdb.last.level.read.bytes", &m.last_level_read_bytes),
        (
            "rocksdb.non.last.level.read.count",
            &m.non_last_level_read_count,
        ),
        (
            "rocksdb.non.last.level.read.bytes",
            &m.non_last_level_read_bytes,
        ),
    ]
}

/// Ticker values from a statistics dump, whose ticker lines read
/// `<name> COUNT : <value>`.
fn parse_tickers(dump: &str) -> HashMap<&str, u64> {
    dump.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(" COUNT : ")?;
            Some((name, value.trim().parse().ok()?))
        })
        .collect()
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
            let tickers = tickers(m);
            loop {
                if let Some(dump) = opts.get_statistics() {
                    let values = parse_tickers(&dump);
                    for (name, counter) in &tickers {
                        if let Some(&v) = values.get(name) {
                            counter.absolute(v);
                        }
                    }
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
    /// [`spawn_exporter`] relies on, and every exported ticker is found by
    /// name.
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

        let dump = opts.get_statistics().unwrap();
        let values = parse_tickers(&dump);
        // Every exported ticker is in the dump under its name.
        for (name, _) in tickers(&RocksdbMetrics::default()) {
            assert!(values.contains_key(name), "{name}");
        }
        assert_eq!(values["rocksdb.number.keys.read"], 4);
        assert!(
            values["rocksdb.block.cache.data.hit"] + values["rocksdb.block.cache.data.miss"] > 0
        );
        // The file reads behind the misses, which rust-rocksdb's Ticker
        // numbering reads as another ticker.
        assert!(
            values["rocksdb.last.level.read.count"] + values["rocksdb.non.last.level.read.count"]
                > 0
        );
        assert!(cache.get_usage() > 0);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
