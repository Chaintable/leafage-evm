# Archive schema v4: an archive in files of its own, without RocksDB

Status: **implemented, not yet in production**. Reads, writes, new v4
archives, an offline v3 → v4 conversion with a verifier, and offline rewind
are implemented and tested against v3 (see "Results").

A v4 archive has no RocksDB and no log. What it has written to files is
all it keeps: a node that stops loses the last blocks it took (36 minutes
of bsc at most) and fetches them again from S3 when it starts, as every
node already does for the 256 blocks of its diff layers.

## Why

A final block's state diff never changes once written. The archive only
appends blocks above its head, and reads a key at a height. RocksDB (v1 – v3)
serves that with machinery the workload does not need: levels to merge,
versions to garbage-collect, a filter and several index levels per file. v4
stores account and storage history in immutable files built for exactly this
read:

- every version of a key next to each other, and the slots of a contract
  next to each other, so one page read answers a lookup and brings its
  neighbours into the cache;
- values stored as differences to the key's previous value;
- an index small enough to stay in memory (about 15 bytes per 12 KiB page),
  so a lookup costs at most one device read and needs no filter.

On bsc this makes the archive 25 % smaller than v3. A node of the production
size answers the same requests with 19 – 21 % less CPU and half the device
reads, or with as many device reads as v3 in half the memory (see
"Results").

## Schema versions

| Version | Account / storage history | Declared by |
|---------|---------------------------|-------------|
| v1 | CFs `4` / `5`, key `key ‖ tail`, ascending block tail | no marker, or `leafage:block_encoding_inverted = 0` |
| v2 | same, descending tail ([ArchiveInvertedBlockEncoding.md](./ArchiveInvertedBlockEncoding.md)) | `leafage:block_encoding_inverted = 1` |
| v3 | CFs `4` / `5`, block as a RocksDB user-defined timestamp ([ArchiveSchemaV3.md](./ArchiveSchemaV3.md)) | file `<db>/LEAFAGE_ARCHIVE_SCHEMA` = `3` |
| v4 | page files in `<db>/v4/` (this document); CFs `4` / `5` do not exist | file `<db>/LEAFAGE_ARCHIVE_SCHEMA` = `4` |

Block infos, the block-number index, the latest pointer and code are in
RocksDB in v1 – v3. A v4 archive keeps them in files of its store too
("Block infos and code"); its directory holds no RocksDB. It is always in
the inverted encoding: the write path encodes keys as v2 does before the
block is split off.

`standalone --archive` and `archive-init` take `--archive-schema v4` for a
new archive (the default stays `v3`). An existing archive is opened by its
schema file, whatever the flag says. A release without v4 support refuses a
v4 archive (unknown schema).

## Logical model (unchanged)

As in v3: history is a set of versions `(key, block, value)`, at most one per
`(key, block)`; `read(key, h)` is the value of the version with the greatest
`block ≤ h`, and that block. An account key is the 32-byte address key, a
storage key the 64-byte `address ‖ slot`. v4 answers every read exactly as
v3, including the block of the version.

Differences in representation only:

- An account version is `(nonce, balance, code hash)` or "deleted"; a code
  hash of zero is stored as `keccak256("")`, which is what readers of v1 – v3
  make of it.
- Block numbers must fit in 32 bits.

## Files

```
<db>/v4/MANIFEST                     the file set (text, replaced atomically)
<db>/v4/base-<id>-a|s.v4  .cidx      base files: accounts | storage
<db>/v4/seg-<id>-a|s.v4   .cidx .bloom   delta segments
<db>/v4/dict-<id>.dict               zstd dictionaries
<db>/v4/blocks.dat  .off  .pfx  .dict    block infos by number
<db>/v4/code.dat    .idx  .dict          contract code by hash
<db>/v4/LOCK                         held (flock) by the process that has the store open
<db>/LEAFAGE_BLOCK_INDEX             saved index from block hash to number (as on v3)
<db>/LEAFAGE_ARCHIVE_SCHEMA          4
```

Page files, dictionaries and filters are written once and never modified.
`MANIFEST` lists the files in use with their block and key ranges, and how
much of the block and code files is valid; it is written aside, synced and
renamed over the old one. **It is the only commit point of the archive.** A
file not in the manifest is a leftover of an interrupted write and is
removed at open; bytes of the block and code files past their valid length
are cut off.

### Page

A page holds versions sorted by key, then block, as eight byte streams
followed one after the other, each prefixed by its length:

| Stream | Content |
|--------|---------|
| kinds | storage only, one byte per record: same key, new slot of the same address, new address |
| keys | each address once, each slot once (accounts: each key once) |
| blocks | varint: the block at a key's first record, else the difference to the previous one |
| tags | storage: value mode (2 bits) and length (6 bits). Accounts: which fields change, deleted, first record of a key |
| nonce | accounts: varint difference to the previous nonce (or the nonce, if it went down) |
| vals | storage values, account balances: the shortest of raw, xor with the previous value, `+difference`, `-difference`, without leading zeros |
| codes | accounts: the code hash, when it changes (`keccak256("")` is the initial one) |
| restarts | one entry every 32 (storage) or 64 (account) records: the block and the offsets into the other streams |

At a restart the value state starts from zero, so a lookup binary-searches
the restart entries and decodes at most one run of records. A page is closed
once it reaches 12 KiB raw; it is then compressed with zstd and the file's
dictionary, with zstd's content checksum, and stored as
`length u32 | frame | zero padding` up to a multiple of 64 bytes. On bsc a
storage record takes 16.2 bytes stored: 9.0 of value, 4.7 of key, 1.6 of
block, 0.9 of the rest.

The page size is a measured optimum, not a constant of nature: with the
memory a production replica has, 6 KiB and 24 KiB pages both read the device
about 17 % more per lookup. Larger pages only look better when the cache
is not full.

### Index (`.cidx`)

One entry per page, all in memory: an 8-byte window of the page's first key,
its first block (u32) and its stored length in 64-byte units (u16). Pages
are grouped in blocks of up to 64; a block header keeps the full first key,
the length `cp` of the prefix all first keys of the block share, the first
page and its file offset. That is 14 bytes per page plus the headers: 15.2
on bsc, 1.9 GiB for the whole archive, read at open (about 18 s at
125 MB/s).

The window of a block inside one contract is the key bytes after the common
prefix. A block spanning contracts takes the rest of the address hash's
first 8 bytes and then the head of the slot hash, so that it also orders the
pages of one contract. A block is closed early when two different first keys
would share a window, or two different addresses the address part of it.
Within a block the windows then order the pages, and a lookup key is
misplaced only among pages that share its window, or the address part of its
window while belonging to another address. The page tells (it holds full
keys), and the lookup then steps to the neighbouring page. This costs a
second read for 1.3 % of the lookups that miss the cache on bsc. Two
contracts whose address hashes agree in their first 8 bytes are served
correctly through the same stepping; making such a pair takes about 2^32
hashes, and only makes lookups in those two contracts slower.

A key with more versions than a page holds starts several pages. These
pages are index blocks of their own, never mixed with the pages of other
keys: in a block spanning contracts they would all have the window of
their address, and a lookup of another slot of that address would step
through each of them (up to 63 reads) to find its page.

The file ends with a CRC32.

## Store

```
            writes ──► memtable ──flush──► segments (L0 ─merge─► L1 ─► …)
                                                │ base rewrite, one base file at a time
                                                ▼
                                           base files (every version up to their block)
```

- The **base** is a list of files per kind in key order, each holding every
  version of its key range up to a block `hi` of its own. A file is closed
  at the first key boundary past 8 GiB, so a key's versions are in one file.
- A **segment** holds every version of a run of blocks, one file per kind,
  with a blocked bloom filter of its keys in memory (10 bits per key). A
  segment whose blocks every base file of its kind holds is deleted.
- The **memtable** holds the blocks not yet in a segment: an arena per
  shard with the versions of a key chained newest first.

### Reads

`read(key, h)` looks in the memtable, then in the segments from the newest
to the oldest whose filter passes (stopping at those the key's base file
already holds), then in the key's base file. The first layer with a version
at or below `h` has the answer, since the layers are ordered by block. A
segment wholly above `h` is not read.

A page lookup also sees the key's next version, when it follows on the same
page, or that there is none. The store returns it with the answer, as the
block below which the answer holds. The archive's state cache
(`ARCHIVE_STATE_CACHE_MB`) records the whole span from it, without the extra
"probe" read v2 and v3 need and often skip (ArchiveSchemaV3.md, "State cache
probes that cannot pay"). The bound is exact in a base file; a segment above
the read height whose filter has the key ends the span at the segment's
first block, without being read.

The store keeps a cache of decompressed pages, and reads the pages it
misses through the kernel's page cache, which holds them as stored: two
levels, the small one without a decompression per lookup, the large one
sized by the kernel to the memory that is free and kept over a restart of
the process. `--db-cache` is the memory of the store itself: the page
index, the filters, the tables of the block and code files and the cache of
block chunks take what they need, the cache of pages the rest. It stays at
what a v3 archive is given (8192 on bsc); the memory beyond it is the
kernel's. An earlier build read with `O_DIRECT` into a cache of its own
that had to be given most of the memory (22000 of 30 GiB in most runs
below); the two were the same in CPU, latency and device reads at the
production rate (Results), and that way of reading is gone.

Merges and base rewrites read files front to back with `O_DIRECT`, and what they write is dropped from the kernel's cache as it
is synced: a pass over the base does not push the pages of the lookups
out.

### Absent slots

A lookup of a slot that a contract never had still reads a page: there is
no filter over the base. On bsc these are 19 % of the lookups and
concentrate on a few very large contracts. A filter over every key would
need 5 – 9 GiB. Earlier builds kept filters of the slots of the addresses
most read for absent slots, chosen online within 256 MiB. They were worth
it for a store that read with `O_DIRECT` into 8 GiB of its own (0.024 →
0.015 device reads per lookup on the prototype); behind the kernel's page
cache the pages they save are in memory, and a node at the production size
read the device as often without them as with them (0.124 – 0.133 against
0.125 per request). They are removed: a manifest that lists some is read,
and their files go at the next open.

### Writes

A commit applies the batch to the memtable: the versions sorted by block,
the infos of its blocks, new code. Nothing else is written. Readers do not
wait for it; they take a block for written when they find its info or see
the head at it, so the order is: the code (found by its hash), then for
each block its versions, its info, the head.

Blocks come in order. A commit of versions at or below the head fails; the
head block itself may be written again while it is still in the active
memtable, as the block it is (the same hash; its versions are written
over). Another block at that number is refused, as the versions of the
first would stay: that needs a rewind. A block the files hold is accepted
again only with the hash they have and without versions (the state tree
writes the head it opened on again). The head is the last block written and
cannot be set to anything else.

**Nothing is in a file, and nothing in a log, until the memtable is
flushed.** A process that stops loses the blocks of its memtable (at most
`ARCHIVE_V4_FLUSH_BLOCKS`, 36 minutes of bsc), opens at the last flushed
block, and fetches the later ones again from S3: a few thousand blocks in
seconds. An earlier design logged them, in a RocksDB column family written
with the block's info. That made a restart exact and was most of what the
node wrote, about 120 of 160 KiB per block: the versions went to RocksDB's
WAL uncompressed, and again to SST files when the CF's write buffer filled
before the next flush. Without it a node catching up wrote 40 KiB per
block, against 274 for v3 on the same blocks.

When the memtable reaches `ARCHIVE_V4_FLUSH_MB` (512) or spans
`ARCHIVE_V4_FLUSH_BLOCKS` (4800) blocks, it is frozen and a background
thread writes it out: a segment per kind, the infos of its blocks appended
to the block files, its code to the code files; all synced, and then the
manifest lists the segments and the new lengths. Writers wait if the
previous memtable is still being written.

Segments merge size-tiered: `ARCHIVE_V4_TIERS` (8) neighbours of a level
become one of the next. On bsc the segments cost 4.7 bytes written per byte
of final segment. While a base rewrite pass is under way, segments it will
drop are not merged with later ones: the merged segment would stay when the
pass ends, with most of the delta in it, and the next pass would be due in
half the time.

### Block infos and code

`blocks.dat` holds the block infos by number in chunks of 16 consecutive
blocks, each a zstd frame with a checksum; a block is `hash ‖ length ‖ info`
with the info encoded as in v1 – v3. `blocks.off` has the place of every
full chunk (8 bytes, all in memory: 66 MB on bsc). The last chunk is in it
once it is full; until then the manifest says where it is, and the next
flush writes it again, longer, behind it. `blocks.pfx` is the first 8 bytes
of the hash of every block, at `(number − first) × 8`: what the index from
hash to number is built from. A block that was never written has a zero
hash and no info.

A block is found by number with one read of about 10 KiB, which has its
hash too; decoded chunks are cached (`ARCHIVE_V4_BLOCK_CACHE_MB`). Requests
name blocks by hash: the index from hash to number of v3's
`ARCHIVE_BLOCK_INDEX` is always on (0.7 GiB on bsc, outside `--db-cache`),
built from `blocks.pfx` and saved to `LEAFAGE_BLOCK_INDEX` as on v3.

`code.dat` holds `hash ‖ zstd frame` per code in the order written;
`code.idx` has hash, offset and length of each. An open reads the index
into a table sorted by the first 8 bytes of the hash (16 bytes per code:
90 MB for the 5.6 M codes of bsc). Code is never removed.

Both are append-only, and their valid lengths are in the manifest: a flush
appends, syncs, and the manifest that lists its segments records the new
lengths. They need no merge and no rewrite. They are read through the
kernel's page cache.

Each has a zstd dictionary (`blocks.dict`, 32 KiB; `code.dict`, 1 MiB)
that the manifest marks as present. A conversion trains them on block infos
and codes spread over the source; an archive that starts empty trains the
first on the blocks of its first flush and the second on its codes once it
holds 20,000, sampled over all of them in the order they were written.
What was written before a dictionary needs none to be read.
Bulk writes compress at zstd level 3; a node following the chain writes a
few megabytes a flush and compresses them at level 19; code is always
compressed at 19, on several threads.

What the format was chosen from, on bsc (bytes per block of recent blocks,
949 raw; bytes per code, 7232 raw):

| Block infos | B/block | | Code | B/code |
|---|---|---|---|---|
| 16 per chunk, level 3 | 623 | | each alone, level 3 | 2992 |
| with a 32 KiB dictionary | 596 | | with a 112 KiB dictionary | 2053 |
| and level 19 | 588 | | with a 1 MiB dictionary | 1824 |
| 64 per chunk, dictionary, level 3 | 591 | | 112 KiB dictionary, level 19 | 1793 |
| 16 per chunk, no hash in the chunk | 618 | | 256 KiB of similar codes together | 1400 |
| | | | 1 MiB of similar codes together | 1282 |

Over the whole chain the files come to 571 bytes per block (bulk, level 3;
older blocks are smaller) and 1524 per code (1 MiB dictionary, level 19).

A block info is mostly what cannot be compressed: three roots, a seal, a
BLS signature and the logs bloom, which zstd brings to 255 bytes where the
entropy of its bits, taken as independent, is 245. The hash kept in the
chunk costs 5 bytes, as it is the parent hash of the next block; the array
of whole hashes that the first version of the format kept for the index
cost 32, more than any encoding gains, and became the 8-byte prefixes.
Larger chunks gain 1 % and make every lookup read four times as much.
Compressing similar codes together would take another quarter off the
code (0.2 % of the archive) and make a lookup decompress a hundred codes
for one.

### Base rewrite

When the delta exceeds `ARCHIVE_V4_DELTA_PERCENT` (4 %) of the base and
`ARCHIVE_V4_DELTA_MIN_MB`, or its filters take more than
`ARCHIVE_V4_DELTA_FILTER_MB` (512), a background thread folds it into the
base, one base file at a time: it merges the file with the versions of its
key range from every segment above the file's block, writes new files, and
swaps them in. Pages no new version falls into are copied without being
decoded (the new file keeps the old one's dictionary). A segment is removed
when every base file of its kind holds it, at the end of the kind's pass:
until then its versions are on disk twice. A pass so needs free space for
the delta once more and for one base file: about 35 GiB on bsc at the
default limits, 55 with `ARCHIVE_V4_DELTA_FILTER_MB=1024`, returned when
the pass ends.

A pass records the block it folds up to in the manifest when it starts
(`pass <kind> <block>`). A node that stops during a pass loses the file
being written and keeps the files already rewritten; when it is due again
(at once: the delta is as large as before) the pass goes on with the files
that are still below that block. The delta flushed since the pass began
stays for the next one.

The rewrite is paced by the node, since the device has no IO scheduler to
do it: at most `ARCHIVE_V4_REWRITE_MBPS` (16) of device IO, read and
written. The rate is all there is: the rewrite does not watch the lookups,
so a pass always ends after `2 × base / rate`. Time in which it did less
than the rate allows (a slow stretch) is not made up for afterwards at the speed of the device. Unthrottled, a rewrite at
200 MB/s on a 125 MB/s volume cut read throughput by 3.5 (measured on the
prototype), so do not raise the rate to the volume's limit on a serving
node. At 16 MB/s a pass over bsc
takes about two days; the delta that accumulates meanwhile is about 2 GiB.

A pass writes the whole base again, so the rewrite costs `base / delta`
bytes written per byte of history, against 4.7 for the segments. On bsc
(a block every 0.45 s, about 800 versions each: 1.9 GiB of history a day)
the filter limit is reached first: the delta's filters take about 20 MiB
per GiB of delta (measured on a node following the chain: 332 MiB for
15.8 GB; on the prototype 993 MiB for 52 GiB), so 512 MiB stands for a
delta of about 26 GiB, 2.2 % of the base and two weeks of blocks. That is
46 bytes written per byte of history, about 90 GiB a day on average, all of
it in the two days of a pass. The rewrite is an estimate from these
settings: no node has run long enough to start one by itself. A production
v3 replica writes 58 – 75 GiB a day to its volume (CloudWatch, November
2026).

To rewrite less often, raise `ARCHIVE_V4_DELTA_FILTER_MB` (and
`ARCHIVE_V4_DELTA_PERCENT` beyond 4): 1024 lets the delta reach 4 %, 2048
about 8 %. The filter limit is reserved from the budget when the store
opens, so each step takes that much from the page cache.

A bulk load (`archive-init`) runs the same store with the rewrite
unthrottled and only when the delta is half of the base, and folds
everything into the base at its end. `archive-v4 compact` does that for any
archive that is not serving.

## Restart and crashes

An open takes the lock, reads the manifest, removes page files it does not
list, cuts the block and code files to the lengths it records, and reads
the indexes and filters. Nothing is recovered: a flush, merge or rewrite
that was interrupted left only what the manifest does not know.

The head of the archive is then the last block of the block files. A node
that stopped, with or without a crash, had blocks above it in its
memtable; they are gone, infos and code included, and it fetches them
again.

The updater therefore keeps **no Kafka offset between runs**, on any
schema: every start catches up from S3 to the end of the Kafka stream and
goes on from there (as a node started from a snapshot always did, and as
`rewind` always forced). `standalone` removes the offset file an earlier
version left, so that a downgraded node does not resume from it.

The index from block hash to number is loaded from its file and completed
from `blocks.pfx`; it is rebuilt from `blocks.pfx` (1 GiB on bsc) when the
file is missing or ends at a block the archive no longer has.

A page or a block chunk that fails its checksum, an index or filter that
fails its CRC, and a file shorter than the manifest or its index says are
errors, not wrong answers: the read fails, or the open does. The tables of
the block and code files (`blocks.off`, `blocks.pfx`, `code.idx`) have no
checksum of their own. What they point at tells: a chunk is a zstd frame
with a checksum, a code is not taken from a place that holds another hash
(an error), and a lookup by hash compares the whole hash of the block it
finds (a damaged prefix makes a block not found, never another returned).

## Rewind

`leafage-evm rewind --archive --to-block N` is offline, as for v2 and v3.
On v4 it does not cut inside a file: **every segment that reaches above
`N` is dropped whole**, with all the blocks it holds, the block files are
cut to match, and the archive ends at the last segment end at or below
`N`. The command reports that block; the node fetches the blocks after it
again. Segments grow by merging, so the archive may go back up to the span
of its largest segment (on bsc at most the two weeks between base
rewrites, an hour or two of catching up).

The base files are not rewound. A target below the block up to which they
hold the history is refused: create the archive again from a snapshot.
After a base rewrite that is everything but its last two days.

`--head-only` is refused: v4 cannot keep versions above its head.

## Conversion from v3

`leafage-evm archive-v4 convert --src <v3> --dst <new dir>` rewrites a v3
archive offline; the source is opened read-only.

- One zstd dictionary per kind is trained on keys sampled over the key
  space.
- Each worker reads a key range of a versioned CF with every version
  (`iter_start_ts`), newest first, turns a key's versions round and writes
  base files. Versions above the source's head are left behind and counted.
  A key with more than 262,144 versions (an account that changes in every
  block has one per block of the chain) is not held whole: the first pass
  notes the block of every 262,144th version, and the key is read again in
  those block windows (`timestamp` / `iter_start_ts`), oldest window first.
  A worker so holds two windows at most, about 80 MB.
- Block infos (from CF `7`, or CFs `3` and `2`) and code (CF `6`) are read
  in order and written to the block and code files; a block number the
  source does not have becomes an absent block. The saved block index is
  copied. Code is compressed at level 19: about 130 codes a second per
  core, two hours for bsc on 6 cores.
- The result has no delta. The schema file is written last: an interrupted
  conversion cannot be opened.

`leafage-evm archive-v4 verify --v3 <v3> --v4 <v4>` reads random keys of
both kinds at heights around each version, and absent keys, and compares
value, block, and that the span v4 reports skips no version. A storage key
is picked in two steps, an address and then a slot of it, so that the slots
inside large contracts are among them (`inner_slots` in the report; the
verification under Results was run before this, and took the first slot of
each address it picked). It opens the v4 store itself: stop the node first.

To move a fleet: convert on the seed, verify, snapshot, and create the
replicas from the snapshot, as for v2 → v3. `--db-cache` stays as it is.

## Configuration

| Variable | Default | Meaning |
|----------|---------|---------|
| `--db-cache` | | memory of the store: page index, filters, the tables of the block and code files, block chunk cache, and its cache of decompressed pages with the rest. As for v3 (8192 on bsc). The memtable and the one being flushed (up to `ARCHIVE_V4_FLUSH_MB` each), the state and code caches and the index from block hash to number are beside it |
| `ARCHIVE_V4_FLUSH_MB` / `ARCHIVE_V4_FLUSH_BLOCKS` | 512 / 4800 | memtable size and age at which it becomes a segment |
| `ARCHIVE_V4_TIERS` | 8 | segments of a level merged into one |
| `ARCHIVE_V4_DELTA_PERCENT` / `ARCHIVE_V4_DELTA_MIN_MB` | 4 / 4096 | delta size, relative to the base and absolute, that starts a base rewrite |
| `ARCHIVE_V4_DELTA_FILTER_MB` | 512 | segment filter memory that starts one |
| `ARCHIVE_V4_REWRITE_MBPS` | 16 | device IO of a base rewrite, read plus written (0: no limit) |
| `ARCHIVE_V4_BASE_FILE_MB` | 8192 | size at which a base file is closed |
| `ARCHIVE_V4_BASE_ZSTD_LEVEL` | 3 | zstd level of base files (15 – 19: about 3 % smaller, 5 – 10 times slower to write) |
| `ARCHIVE_V4_BLOCK_CACHE_MB` | 64 | memory for decoded chunks of block infos |

`ARCHIVE_STATE_CACHE_MB` and `ARCHIVE_CODE_CACHE_MB` work as on v3. The
block index is always on (`ARCHIVE_BLOCK_INDEX` is not read), and the
`ROCKSDB_*` variables have nothing to act on.

## Metrics

`leafage_archive_v4_*`: `page_reads`, `page_read_bytes`, `extra_pages`,
`memtable_hits`, `segment_probes`, `segment_hits`, `flushes`, `merges`, `rewrites`, `rewrite_bytes` (counters);
`memtable_versions`, `segments`, `delta_bytes`, `base_files`, `base_bytes`,
`index_bytes`, `segment_filter_bytes`, `page_cache_capacity_bytes`, `page_cache_usage_bytes`, `durable_block`
(gauges). The read latencies, state cache counters and
`archive_{account,storage}_gets_{found,missing}` of `leafage_storage_*`
count v4 reads too. There are no `leafage_rocksdb_*` metrics on a v4
archive.

## Not supported on v4

- `db-migrate` latest-state scans from an archive (as on v3), and its scan
  of the code: they return an error.
- `archive-clear-deleted`, `archive-scan`, `force-compact`, `put-code`,
  `pipeline-export`: they work on a RocksDB, which a v4 archive does not
  have. Convert an archive after repairing it.
- `rewind --head-only`, and a rewind into the base (above).
- Block numbers of 2^32 and above.
- An exact restart: see "Writes".
- Archives of development builds that kept block infos and code in a
  RocksDB next to the store: convert the v3 source again.

## Results (bsc)

Measured on a bench host (12 cores, 93 GiB) with the archive on gp3 volumes,
bsc through block 131,404,227, one binary for both schemas.

### Conversion and size

`archive-v4 convert` with 12 workers took 4 h 25 min and at most 18.8 GiB of
memory: 32.67 G account versions and 72.81 G storage versions into 200 base
files. Most of the last hour is one worker on the key range of the largest
contract; ranges are cut by the first key byte, not by size.

| | v3 | v4 |
|---|---|---|
| Whole archive | 1708 GiB | 1285 GiB (−24.8 %) |
| Account and storage history | about 1619 GiB | 1195 GiB (account 175, storage 1019) |
| Bytes per version | about 16.5 | 12.2 (account 5.75, storage 15.0) |
| Block infos, block numbers, code | about 90 GiB | 90 GiB as RocksDB CFs, 80 GiB in files (below) |
| Index in memory | (in the block cache) | 1.87 GiB, loaded in 18 s at open |

The v3 history size is the whole archive less the other CFs. This
conversion still copied them into a RocksDB beside the store; the node then
moved them into the block and code files (below). With both nodes following
the chain, at block 133,000,504, the v3 archive is 1739 GiB and the v4
archive 1292 GiB (−25.7 %).

`archive-v4 verify` read 40,000 random keys at 1,080,000 heights and 40,000
absent keys in 13 min: no difference from v3.

### Block infos and code against RocksDB

The same 132,964,228 block infos and 5,621,228 codes, in v3's column
families and in v4's files:

| | RocksDB (v3) | v4 files |
|---|---|---|
| Block infos by number | 70.36 GiB | `blocks.dat` 70.73 GiB (571 B per block) |
| From hash to number | 5.93 GiB | `blocks.pfx` 0.99 GiB, `blocks.off` 0.06 GiB |
| Code | 13.75 GiB | `code.dat` 7.98 GiB (1524 B per code), `code.idx` 0.23 GiB |
| Together | 90.04 GiB | 79.99 GiB (−11 %) |

The block infos themselves do not shrink (they are bloom filters, roots and
signatures); the index by hash does, and the code, which RocksDB compresses
in blocks of mixed content and v4 one by one against a dictionary of code.

2000 lookups of each kind through the RPC of two nodes at the same head,
after the kernel's page cache was dropped and with blocks neither node had
read before; time in the storage call and device reads per lookup:

| | v3 | v4 |
|---|---|---|
| Block by number, cold | 1.99 ms, 3.1 reads | 0.56 ms, 1.02 reads |
| Block by hash, cold | 0.99 ms, 1.42 reads | 0.54 ms, 1.02 reads |
| 64 blocks in a row, cold, per block | 0.22 ms, 0.15 reads | 0.06 ms, 0.08 reads |
| Code of a contract, cold | 1.59 ms, 2.7 reads | 0.68 ms, 1.14 reads |
| Block by number, again | 33 µs | 3 µs |
| Block by hash, again | 25 µs | 36 µs |

The code row's reads include the account lookup of the request. v3 asks two
column families for a block by number (the info, and the hash for the
answer); v4's chunk has both. The codes of the 2000 contracts were the same
on both nodes.

Moving them out of RocksDB at the first open took 2 h 54 min on the node's
6 CPUs: 43 minutes for the block infos (level 3), the rest for the code at
level 19 (760 codes a second).

### Following the chain

Two nodes of one binary at the production size (6 CPUs, 30 GiB, the volume
throttled to 8000 IOPS and 125 MB/s) on the production pipeline, at the
head and serving nothing; per-minute device and cgroup counters:

| | v3 | v4 |
|---|---|---|
| Written to the device, 3 h (24,195 blocks) | 6.25 GiB, 271 KiB/block | 0.74 GiB, 32 KiB/block |
| The same, per day | 50 GiB (71 over 6.6 h) | 5.9 GiB |
| Write requests per day | 536,000 | 29,000 |
| CPU | 0.035 cores | 0.012 cores |
| Process memory | 10.7 GiB | 4.0 GiB (page cache of the store nearly empty) |
| Archive growth | within its compaction noise | 1.7 GiB/day |

v3's writes come in bursts of compaction: the three hours before these
wrote 84 GiB a day, the 6.6 hours together 71, which is what the
production replicas write. v4 flushed five times (one segment per kind and
36 minutes, 2 – 4 M versions in 2 – 4 s) and merged eight segments into one
once. No base rewrite ran, and none is due for two weeks; averaged over
its period it adds the 90 GiB a day estimated under *Base rewrite*, or 48
with `ARCHIVE_V4_DELTA_FILTER_MB=1024` (a pass every 25 days, when the
delta is 4 % of the base), in the two days of each pass.

### Lookups alone

`archive-v4 replay-gets` replays the 90.4 M RocksDB Gets that a v3 node made
for 619 s of one production pod's traffic (recorded without the state cache)
on 16 threads, and compares the version each one finds with the one v3
found. The device is throttled to the production volume (8000 IOPS,
125 MB/s). The last 30 M Gets:

| Budget | Gets/s | Device reads per Get | CPU per Get | p50 / p99 / p99.9 |
|---|---|---|---|---|
| 28 GiB | 1,147,000 | 0.0069 | 3.4 µs | 2 / 22 / 1136 µs |
| 8 GiB | 255,000 | 0.031 | 4.1 µs | 2 / 659 / 1268 µs |

No Get of either run found another version than v3. The first run chose 481
address filters (280 MiB) from the absent reads it saw, as builds did
then ("Absent slots"); the later runs, the node runs included, start with
them.

### A node at the production size

The same pod's requests (5.88 M, almost all `contractMultiCall`) replayed
against a node in a container of 6 CPUs with the device throttled as above:
2.35 M requests as fast as 16 connections take them from a cold start, then
the other 3.53 M at a fixed rate. v3 runs as in production (`--db-cache`
8192, reads through the page cache, 30 GiB), v4 as it read when these were
run: with `O_DIRECT` into its own cache, `--db-cache` 22000 in 30 GiB and
10000 in 16 GiB.

At 4500 requests/s, the rate of a production pod (785 s):

| | v3, 30 GiB | v4, 30 GiB | v4, 16 GiB |
|---|---|---|---|
| CPU ms / request | 0.362 | 0.293 (−19 %) | 0.293 (−19 %) |
| Latency p50 / p90 ms | 0.41 / 1.26 | 0.35 / 0.99 | 0.35 / 1.02 |
| Latency p99 / p99.9 ms | 14.7 / 36.3 | 10.1 / 28.6 | 10.9 / 31.7 |
| Device reads / request | 0.253 | 0.125 (−51 %) | 0.249 |
| Read from the device | 8.5 GiB | 5.9 GiB | 12.2 GiB |
| History lookups / request (after the state cache) | 5.03 | 4.22 | 4.22 |
| State cache hits, storage | 80.1 % | 85.0 % | 85.0 % |
| Memory: process + page cache | 10.1 + 19.3 GiB | 21.6 GiB | 11.1 GiB |
| Cold start, requests/s on 16 connections | 9,800 | 15,200 | 13,500 |

At 9300 requests/s, the rate of the capture (380 s):

| | v3, 30 GiB | v4, 30 GiB |
|---|---|---|
| CPU ms / request | 0.350 | 0.278 (−21 %) |
| Cores used | 3.25 | 2.59 |
| Latency p50 / p90 ms | 0.52 / 3.89 | 0.40 / 1.71 |
| Latency p99 / p99.9 ms | 30.4 / 59.8 | 20.1 / 39.7 |
| Largest send lag | 222 ms | 48 ms |

Without the state cache (`ARCHIVE_STATE_CACHE_MB=0`, 4500 requests/s, 15
history lookups per request) CPU is 0.434 against 0.311 ms per request
(−28 %): the cost of a lookup itself. With it, v4 also makes a sixth fewer
lookups, because a read tells the cache how long its answer holds without a
second lookup.

v3's process and page cache fill the 30 GiB by the end of a run. v4's page
cache of 18.4 GiB ends 95 % full and has evicted nothing, so its device
reads are first reads of a page; the ten minutes of traffic are too short
to show either schema in a steady state at this size. The 16 GiB node's
cache of 6.6 GiB is full from the warm-up on: it reads the device as often
as v3 in 30 GiB, in more bytes (a 12 KiB page per read; 16 MB/s of the
volume's 125), at the CPU and latency of the 30 GiB node.

A v3 node and a v4 node side by side answered 480,000 of the captured
requests (every twelfth; 2,477,401 calls) the same but for the time each
call reports it took.

The runs above are of a build that still kept block infos and code in
RocksDB, on nodes that did not follow the chain. The final build, both
schemas at the head on the production pipeline while they serve the same
capture (each node restarted with cold caches before its run; a delta of
18 GiB in 16 – 28 segments on v4, which had no address filters and was
building them during the first run):

| | v3, 4500/s | v4, 4500/s | v3, 9300/s | v4, 9300/s |
|---|---|---|---|---|
| CPU ms / request | 0.376 | 0.316 (−16 %) | 0.363 | 0.284 (−22 %) |
| Cores used | 1.69 | 1.42 | 3.38 | 2.64 |
| Latency p50 / p90 ms | 0.43 / 1.47 | 0.36 / 1.01 | 0.60 / 9.92 | 0.40 / 2.37 |
| Latency p99 / p99.9 ms | 17.3 / 40.0 | 11.5 / 35.5 | 42.3 / 74.7 | 28.4 / 54.6 |
| Largest send lag | 36 ms | 32 ms | 607 ms | 157 ms |
| Device reads / request | 0.280 | 0.133 (−53 %) | 0.295 | 0.124 (−58 %) |
| Read from the device | 9.3 GiB | 13.3 GiB | 9.8 GiB | 6.9 GiB |
| Blocks synced meanwhile | 1869 | 1865 | 968 | 962 |

v4's first run read 4.1 GiB for lookups; the rest is a round of address
filter building that started in it (this build still had them) and a
segment merge.

With its 527 address filters built again and no filter round in the
window, the v4 node at 4500 requests/s, as above and with page files read
through the kernel page cache (`--db-cache=8192`: a small cache of
decompressed pages over the kernel's cache of the files; since then the
only way):

| | `O_DIRECT`, `--db-cache=22000` | buffered, `--db-cache=8192` |
|---|---|---|
| CPU ms / request | 0.298 | 0.298 |
| Latency p50 / p90 ms | 0.35 / 0.95 | 0.35 / 0.91 |
| Latency p99 / p99.9 ms | 9.67 / 27.7 | 9.57 / 29.9 |
| Device reads / request | 0.123 | 0.113 |
| Read from the device | 5.8 GiB | 5.8 GiB |
| Pages the store read / request | 0.121 | 0.275 (0.162 from the kernel's cache) |
| Memory: process + page cache | 22.2 + 3.2 GiB | 10.1 + 17.5 GiB |
| Cold start, requests/s on 16 connections | 13,900 | 14,400 |

The two are the same at this load: the store misses its own cache more
than twice as often, and a miss that the kernel has costs too little to
show. Against v3 in the same conditions that is −21 % CPU, −44 % p99 and
−56 % device reads. The buffered arrangement leaves the sizing to the
kernel, keeps the cache over a restart of the process and needs no other
`--db-cache` than v3's, which is why it was kept and the other removed;
neither run filled the 30 GiB, so they do not show which evicts better.

The same run on the builds that followed, each on the archive the one
before left (`--db-cache=8192`, lookups through the kernel page cache
without readahead):

| | with address filters | filters, a round building 125 in the window | without (final) |
|---|---|---|---|
| CPU ms / request | 0.303 | 0.321 | 0.304 |
| Latency p50 / p90 ms | 0.35 / 0.91 | 0.37 / 0.98 | 0.35 / 0.91 |
| Latency p99 / p99.9 ms | 8.69 / 26.7 | 10.98 / 34.4 | 8.70 / 24.5 |
| Device reads / request | 0.122 | 0.126 | 0.122 |
| Read from the device | 4.6 GiB | 7.4 GiB | 4.4 GiB |
| Pages the store read / request | 0.275 | 0.280 | 0.330 |

Without the filters the store asks for a fifth more pages, and the kernel
has them: the device is read as often. A round of filter building in the
window cost more than the filters ever saved here. Without readahead the
device reads are as many as with `O_DIRECT` and a fifth fewer bytes.
Against v3 (0.376 ms, p99 17.3 ms, 0.280 reads per request) the final
build is at −19 % CPU, −50 % p99 and −56 % device reads.

Not measured here: a base rewrite on a serving node (covered by tests and
by the store-level runs of the study), and more than ten minutes of
traffic.
