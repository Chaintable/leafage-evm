# Complete archive state-cache intervals

Schema v3 previously cached a historical version only through the requested
height when a newer version existed at the head. Reading that same version at
successively higher heights repeatedly called RocksDB. For example, a value
written at block 10 and next written at 100 stayed cached as `[10, 16]` after
reads at 15 and 16, even though it was valid through 99.

On the second miss for the same version, a bounded timestamp-range iterator
now finds the **first** newer version and extends the span to `[10, 99]`.
If no newer version exists in the captured read view, it extends to that view's
head. Absence and empty deletion/zero values follow the same rule. Exact-probe errors
retain the original successful read and its unextended span.

The shared `udt_record` path handles scalar and batch account/storage reads.
Cold keys, head reads and volatile keys do not do the extra seek. Misses on a
different version retain the existing head-Get policy. There is no schema change,
new dependency or new `unsafe`. The per-key cap remains **four version spans**;
the entry layout, cache byte budget and eviction policy are unchanged. Extending
`hi` stores no additional values or per-block entries. The probe's iterator is
dropped immediately; it is not retained by the cache.

## Benchmark

Measured 2026-11-10 on macOS/aarch64, Rust 1.97.0-nightly (e95e73209), release
profile, against `c7e43b3e5e9efeb32413063138fbbc90037e27fe`. Both binaries used
the same committed `state_cache_bench.rs`; the baseline had no production changes.
Six runs per binary, alternating baseline/optimized order. Medians below are
**ns per StateDB read**, including correctness assertions. Raw measurements:
[archive-state-intervals.csv](archive-state-intervals.csv).

| Workload | Storage before → after | Change | Account before → after | Change |
| --- | ---: | ---: | ---: | ---: |
| Same version, 2 reads/key | 1194.66 → 2273.07 | +90.27% | 1258.16 → 2204.34 | +75.20% |
| Same version, 4 reads/key | 1178.02 → 1110.26 | −5.75% | 1247.55 → 1103.78 | −11.52% |
| Same version, 8 reads/key | 1183.67 → 583.91 | −50.67% | 1228.42 → 560.92 | −54.34% |
| Same version, 16 reads/key | 1184.65 → 295.52 | −75.05% | 1215.05 → 288.88 | −76.22% |
| Same version, 1024 reads/key | 1173.78 → 21.49 | −98.17% | 1237.07 → 22.38 | −98.19% |
| Head cache hits | 16.14 → 16.74 | +3.69% | 16.95 → 17.29 | +1.97% |
| Different version every read | 1208.63 → 1239.08 | +2.52% | 1273.09 → 1262.85 | −0.80% |

Fixture: real RocksDB v3, flushed and compacted SSTs, 512 keys per group, 64 MiB
RocksDB block cache and 16 MiB state cache. Sparse keys have versions at 100 and
10000, with a head of 20000. Each independent trace starts with an empty state
cache, seeds the head version, then reads increasing heights starting at 100.
Short cases (2/4/8/16) repeat 64 independent traces. The long case and controls
use one trace; the controls make 64 reads/key. Dense keys change every block
from 100 through 163, plus a version at 10000. Both CFs are warmed before timing.
Database setup, compaction, cache resets, head seeding and reader construction
are excluded from timing. Reads execute on one thread with no metrics exporter.

The head seed lets the baseline skip its optional head Get: it already knows a
newer version exists. The two-read regression therefore compares two Gets with
two Gets plus one boundary seek, not every possible two-read workload.

The extra seek loses on two-read traces; roughly four reads amortized it in this
fixture, with a small/noisy margin there. Long reuse shows the potential ceiling,
not a typical RPC speedup. Head samples are sub-millisecond, and the dense
control has outliers; their small differences are not evidence of a benefit.
This is a warm-cache storage microbenchmark, **not** cold-disk IO, EVM execution,
production traffic replay or a measured `contractMultiCall` latency improvement.
Repeated reads at exactly the same height already hit the old cache.

Run on either revision with the same benchmark file and its `#[cfg(test)]`
module declaration copied into the baseline checkout:

```sh
cargo test -p leafage-evm-storage --release --lib bench_archive_state_intervals \
  --locked -- --ignored --nocapture --test-threads=1
```

Build each revision before timing, keep both test executables, run the above
test filter six times per executable in alternating order, and divide each
`INTERVAL_BENCH` row's elapsed nanoseconds by its read count. Use the median of
the six samples. Do not compare a debug build or include compile time.

## Validation and observability

The existing v3/v2 differential tests cover scalar/batch values and rewind.
Focused tests cover exact interval endpoints, deletion/recreation, never-written
keys, probe-error fallback, first/repeated/volatile/head gating, and both CF key
lengths at memtable, flushed and compacted stages (including all-`ff` keys and
the maximum timestamp). The full storage library suite passes: 89 tests,
2 intentionally ignored benchmarks, no failures.
The `leafage-evm-rpc` `blockx_batch` integration suite also passes all 3 tests
(requires permission to bind localhost for its test servers).

Existing account/storage hit/miss counters measure whether the learned spans
are used. New `leafage_storage_state_cache_interval_probe_latency` histogram
`_count`/`_sum` measure probe frequency and cost; the
`leafage_storage_state_cache_interval_probe_errors` counter tracks failed probes.
The existing probe counters include these exact probes. Compare saved point-read
time against the additional probe time on comparable traffic before claiming a
net RPC improvement; a high miss rate alone does not establish reusable intervals.
