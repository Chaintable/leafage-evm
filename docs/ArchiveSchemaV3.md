# Archive schema v3: versions as RocksDB user-defined timestamps

Status: **prototype**. Reads, writes, new v3 archives, an offline v2 → v3
conversion and offline rewind are implemented.

## Schema versions

| Version | Account / storage history (CFs `4` / `5`) | Declared by |
|---------|-------------------------------------------|-------------|
| v1 | key `key ‖ tail`, 32-byte ascending block tail, read with `SeekForPrev` | no marker, or `leafage:block_encoding_inverted = 0` |
| v2 | same, descending tail `u64::MAX - block`, forward `Seek` (#162, [ArchiveInvertedBlockEncoding.md](./ArchiveInvertedBlockEncoding.md)) | `leafage:block_encoding_inverted = 1` |
| v3 | key alone, block as a RocksDB **user-defined timestamp**, point `Get` at a read timestamp (this document) | file `<db>/LEAFAGE_ARCHIVE_SCHEMA` = `3` |

Block info, the block-number index, the latest pointer and code are the same
in every version.

## Logical model (unchanged)

1. An account key is the 32-byte address key; a storage key is the 64-byte
   `address ‖ slot`.
2. History is a set of versions `(key, block, value)`, at most one per
   `(key, block)`.
3. `read(key, h)` is the value of the version with the greatest
   `block ≤ h`, and that block; no such version: the account is absent, the
   slot is zero.
4. An account value is `RLP(SlimAccount)`, empty when the account was
   deleted. A storage value is a big-endian 256-bit integer; v3 stores it
   without leading zeros (zero is empty). Readers accept both forms.
5. The archive only appends final blocks above its head.

v3 answers every read exactly as v2, including the block of the version.

## Layout

The versioned CFs keep their names. Each version is one RocksDB entry:

```
user key    address (CF 4)  |  address ‖ slot (CF 5)
timestamp   block, u64 fixed64 (little-endian), 8 bytes
value       as in the logical model
```

The CFs use RocksDB's built-in comparator `leveldb.BytewiseComparator.u64ts`:
bytewise on the key, then **descending** timestamp, so a key's versions sort
newest first: the order of v2. It is loaded by name
(`comparator=leveldb.BytewiseComparator.u64ts`), since rust-rocksdb only
takes a Rust callback comparator.

Table options are the v2 ones (partitioned index and filters, hybrid
ribbon filter, LZ4 / ZSTD per level) except for filtering: there is no
prefix extractor, and filters are **whole-key** over the key without its
timestamp, one entry per key whatever its number of versions.

A CF's comparator is fixed when it is created, so the database declares its
schema in `LEAFAGE_ARCHIVE_SCHEMA` and is opened with the matching options.
Opening with the wrong options fails in RocksDB (comparator name mismatch);
an older binary cannot open a v3 archive.

### Choosing the schema of a new archive

`standalone --archive` and `archive-init` take `--archive-schema v1|v2|v3`
(default `v3`), used only when `--db-path` holds no RocksDB database yet
(no `CURRENT` file): v3 writes the schema file before the open creates the
column families, and every new archive records its encoding marker (v1: 0;
v2 and v3: 1, as v3 keys are encoded as v2's before the block is split
off). `--archive-schema v1` with `--inverted-block-encoding` is refused.

An existing archive is opened as before, whatever the flag says: the schema
file selects v3, otherwise the encoding marker (or, for an unmarked one,
`--inverted-block-encoding`) selects v1 or v2. MDBX archives ignore the flag.

`archive-init` bulk-loads v1 / v2 versions as SST files that parallel
workers ingest batch by batch. A v3 archive cannot take them: RocksDB only
ingests a file into a timestamped CF if its keys do not overlap the CF's,
and a key's versions must reach the database in block order, since a read
stops at the first level holding a version at or below its height. On v3,
`archive-init` puts each batch's versions into the batch's write batch,
which is committed in block order with its block info. This is slower than
ingestion; a large chain is better initialized as v2 and converted.

## Reads

A state handle at height `h` owns read options with timestamp `h` and no
iterator. `read(key, h)` is one `Get` on the CF: RocksDB returns the newest
entry of `key` with timestamp `≤ h` and its timestamp, which is the version
and its block. Compared with v2's `Seek`:

- No iterator: no per-level iterator creation, no merging heap, no
  `DBIter`. The handle holds nothing for the iterator tracker to time out.
- `Get` walks the levels top-down and stops at the first one holding a
  version `≤ h`; a `Seek` positions every level whose filter passes, then
  merges.
- Each SST records its timestamp range (`rocksdb.timestamp_min/max`). A
  `Get` at `h` skips a file whose `timestamp_min > h` before its filter or
  index: recent files are free for historical reads.

rust-rocksdb has no `Get` that returns the timestamp, so reads call the C
API `rocksdb_get_cf_with_ts` directly (`archive/udt.rs`). A version is found
iff a timestamp comes back (an empty value may come back as a null
pointer).

`read_storage_slots(address)` at `h` is an iterator over the address prefix
with read timestamp `h`: it yields each slot once, at its version `≤ h`.

The cross-request state cache (`ARCHIVE_STATE_CACHE_MB`) works on v3 as on
v2. A read only proves its span up to the read height, so a second miss on
a key probes with a `Get` at the cache view's head: if the newest version
there is the one just read (or, for a key never written, there is none),
the value holds up to the head; otherwise that newer version bounds the
key's next version from above, and a key whose bound is fewer than 64
blocks away is not probed again. A probe that cannot extend the span is
skipped: a read at the head already reaches it, and a key whose cache entry
already knows a version above the one read (from an earlier probe or read)
would only find that version again, since the archive only appends. A
backfill reads a changing key at ever higher blocks, each time a version
older than the head's, so only its first miss probes. A `Get` costs about
what a cache lookup and insert do, so the cache saves little CPU on v3, but
it halves the RocksDB reads per request (measured below). In production
the version without it ran out of disk IOPS, see "Production, one pod".

## Writes

A block's account and storage writes are put with their block as the
timestamp (`put_cf_with_ts`); the last write of a block per key wins as
before. RocksDB requires a key's timestamps to grow with its sequence
numbers, which holds: the archive writes blocks in order.

No full-history GC is configured (`full_history_ts_low` is never set), so
compaction keeps every version.

## Conversion from v2

`leafage-evm archive-udt convert --src <v2> --dst <new dir>` rewrites a v2
archive offline; the source is opened read-only and must carry the inverted
marker.

- v2 and v3 sort versions identically, so each worker streams a key range
  straight into SST files: the key without its tail, `u64::MAX - tail` as the
  timestamp, storage values without leading zeros. Other CFs are copied.
- Files roll only between keys, so no two files share a key; all of them
  ingest into the bottom level.
- Files are written with the destination CFs' own options (filters,
  partitioned index, bottom-level compression), so no compaction follows.
- The schema file is written last: an interrupted conversion cannot be
  opened as v3.

`leafage-evm archive-udt verify --v2 <v2> --v3 <v3>` reads random keys of
both versioned CFs at heights around each version, and absent keys, and
compares value and block.

## Rewind

Rewind is offline (`leafage-evm rewind --archive`). v2 deletes the versions
above the target; v3 cannot use `Delete`, which writes a deletion
**version**: a tombstone at `(key, b)` hides the older versions for reads at
`h ≥ b`. A compaction filter has the same problem above the bottom level.

v3 drops them with RocksDB's `OpenAndTrimHistory` (C API
`rocksdb_open_and_trim_history`), which opens the database and runs a forced
full compaction of every timestamped CF, keeping the entries with timestamp
`≤ target`:

1. Check on a read-only open that the target is in the block index and not
   above the head.
2. Remove the Kafka offset file, as v2 rewind does.
3. Trim CFs `4` / `5` to the target and close.
4. Open as usual and truncate the block index and block info, then move
   the head (the v2 steps, without the versioned CFs).

Interrupted after step 3, a rerun trims again: a compaction with nothing to
drop, still a full rewrite.

The trim opens with automatic compactions disabled. Its bottommost pass
(`kForceOptimized`) skips files numbered after the trim started, so an
automatic compaction running meanwhile (scheduled by the open itself) would
write untrimmed files that the trim never visits, leaving versions above the
target.

Cost: the trim rewrites the whole versioned CFs, with 8 compaction threads
(`max_subcompactions`; the serving archive uses 4). On bsc (about 2.4 TB) the
estimate is about 4.5 hours when CPU-bound, like the conversion, and over 10
hours at 125 MB/s; not measured yet. v2 rewind is no faster: its single
thread scans the versioned CFs at about 1.4 M storage / 1.75 M account
entries per second, CPU-bound, about 19 hours on bsc.

A cheaper rewind that leaves the versioned CFs alone and repairs the fork's
versions while the new chain is synced is possible, but needs a stale-version
list and a change to the commit path; it is left for later.

## Not supported on v3 yet

`db-migrate` latest-state scans from an archive, and `archive-scan` on the
versioned CFs. RocksDB rejects an untimestamped read of a v3 CF, so these
fail instead of reading wrong data. `force-compact` is untested on v3.

## Prototype results (bsc)

bsc archive, 71.3 G storage and 32.1 G account versions, on gp3 volumes
(8 cores, 8 jobs, 1000 MB/s):

| | v2 | v3 |
|---|---|---|
| Database size | 2397 GiB | 2053 GiB (−14.4 %) |
| Storage version (data + index + filter) | about 24.6 B | 22.7 B |
| Account version | | 16.0 B |

- Conversion: 5 h 10 min, CPU-bound; the last storage key range ran alone
  for about 40 minutes (ranges are split by first key byte, not by size).
  14 893 SST files, all at the bottom level, filters present.
- `verify --samples 50000`: 100 000 keys, 2.6 M reads, 0 mismatches.
- 8 580 captured production responses served from v3: 0 mismatches.

Serving, same binary, gp3 at 125 MB/s / 8000 IOPS each, 4 cores, three
captured production segments (2026-10-22), second (warm) pass. These runs
predate dropping the state cache on v3: the "v3" columns have it on, the
"no state cache" columns are what v3 runs now.

| | v2 | v3 | v2, no state cache | v3, no state cache |
|---|---|---|---|---|
| CPU ms / request at 1100 rps | 1.23 | 1.04 (−15 %) | 1.52 | 1.00 (−35 %) |
| CPU ms / request, saturated | 1.65 | 1.24 (−25 %) | 2.01 | 1.20 (−40 %) |
| Saturated rps (4 cores) | 2430 | 3240 | 2000 | 3340 |
| p50 / p99 ms at 1100 rps | 0.91 / 7.8 | 0.80 / 6.7 | 1.02 / 7.8 | 0.77 / 6.0 |
| RocksDB share of CPU samples | 48 % | 32 % | 63 % | 41 % |
| RSS | 8.8 GiB | 4.4 GiB | 8.7 GiB | 4.3 GiB |

RocksDB CPU drops by about half: iterator creation, merging and `DBIter`
are gone, and filters cost half as much. Index-block lookups are the largest
part left (11–15 % of samples).

Without probes, the state cache hits half as often on v3 (storage 27 %
against 54 %), since spans end at the reader's height, and v3 is no faster
with it than without it. With the head probe (same setup, a later run):

| | v2 | v3, probing | v3, no state cache |
|---|---|---|---|
| CPU ms / request at 1100 rps | 1.27 | 0.99 | 0.97 |
| Storage hits / RocksDB storage reads per request | 54 % / 16.8 | 47 % / 19.6 | – / 36.8 |
| p50 / p99 ms | 0.95 / 8.1 | 0.76 / 6.7 | 0.75 / 5.4 |

The probe brings the hit rate close to v2's, but the cache still does not
pay on v3: it saves about a fifth of the RocksDB samples, and moka costs
as much (8 % of samples). A v3 `Get` costs about what a cache lookup and
insert do.

### Production traffic of 2026-10-29

Captured for 10 minutes on the 6 busiest bundle machines of the two bsc
backfills then running (96 % of bsc archive multicalls: `price.token_parse`
at blocks below 20 M, and a backfill around 121 M), one pod's 1/22 sample
(267 k requests, 5.2 calls per multicall), 8 cores as in production, open
loop at 9300 rps (production per-pod rate at capture time), second pass:

| | v2 | v3 | v3, no state cache |
|---|---|---|---|
| CPU ms / request | 0.74 | 0.52 (−30 %) | 0.48 (−35 %) |
| Cores used | 6.7 | 4.8 | 4.5 |
| p50 / p99 ms | 11–26 / 64–83 | 0.7–1.2 / 10–37 | 0.6–0.9 / 8–16 |
| RocksDB reads / request (storage + account) | 4.2 + 2.9 | 4.4 + 2.9 | 11.2 + 3.8 |
| RSS | 9.2 GiB | 8.0 GiB | 7.6 GiB |

v2 queues at this rate (p50 above 10 ms); v3 does not. Serial per-request
latency on an unseen 30 k-request sample (warm page caches), by v2's own
ranking:

| | v2 | v3 | v3, no state cache |
|---|---|---|---|
| All | 0.87 ms | 0.56 (−35 %) | 0.54 (−37 %) |
| Slowest 1 % | 8.4 ms | 2.6 (−69 %) | 2.4 (−72 %) |
| Slowest 0.1 % | 26.9 ms | 4.3 (−84 %) | 3.5 (−87 %) |

The heaviest requests gain the most. Without the state cache v3 is a
little cheaper in CPU, but it reads RocksDB more than twice as often.

### Production, one pod

v3 without the state cache served as one of 22 bsc archive pods
(2026-10-29/30, gp3 8000 IOPS / 125 MB/s like the others), against v2
pods on the same traffic. CPU per request was about 40 % lower and p99
stayed near 5 ms while v2 queued (up to 450 ms at 9–15 k requests per pod
and second). But it read the disk more per request than v2 (up to 1.55
against 1.0 reads), and on a read-heavy stretch it hit the volume's 8000
IOPS for 10 minutes: mean latency rose to 50–100 ms while v2 stayed near
2 ms. That brought the state cache back on v3.

### State cache probes that cannot pay

On that traffic the head probe was mostly wasted: the backfills read keys
far below the head, where a changing key's newest version is above the one
read, so the probe finds it and cannot extend the span. With quick_cache
(#280) that was 4.9 probes per request, 90 % of them finding a newer
version. Skipping a probe when the cache entry already knows a version
above the one read (or the read is at the head) leaves 0.6, with the same
hit rate.

Same capture and setup as above (8 cores, 9300 rps, 30 GiB, warm page
cache), on a fresh v3 volume from the 10-29 snapshot, mean of passes 2–3:

| | no state cache | #280 | probe skipping |
|---|---|---|---|
| CPU ms / request | 0.443 | 0.447 | 0.400 (−10 %) |
| RocksDB `Get`s / request, probes included | 15.0 | 12.2 | 7.6 |
| Storage hits | – | 60.6 % | 63.1 % |
| p99 ms | 4.3–5.5 | 3.3–5.3 | 2.9–3.3 |

The cache does not lower disk reads in a short run, though. With 12 GiB
and the host page cache dropped before each run, all three read the disk
about 2.7–2.8 times per request and stall at the volume's 8000 IOPS
(~2950 rps, p50 ~42 ms); the cache's hits are hot keys whose blocks the
8 GiB block cache holds anyway. Whether it saves disk reads in steady
state, where the keys read again outnumber what the block cache holds, is
left to a production canary.
