# Leafage Rings: archive history in append-only rings

Status: draft, on hold. Nothing here is implemented. The measured v3 numbers
below show two problems with the ring layout (see [Risks](#risks)); measure
them before writing any code (see [Measure first](#measure-first)).

## Why

Schema v3 ([ArchiveSchemaV3.md](ArchiveSchemaV3.md)) is close to what RocksDB
allows, but RocksDB is built for mutable data and the archive's history never
changes once a block is final. On bsc (11-2026):

- **Size.** 1.7TB after ZSTD, on each of 22 replicas. Measured on the bsc v3
  archive (table properties, 11-2026):

  | | storage (CF 5) | account (CF 4) |
  |---|---|---|
  | size | 1.42TB | 0.37TB |
  | versions | 72.8B | 32.7B |
  | bytes per version: data / index / filter | 18.6 / 0.85 / 0.12 | 10.6 / 0.82 / 0.03 |
  | raw key / value | 80 (key 64, ts 8, seq 8) / 12.2 | 48 / 40.7 |
  | distinct keys (approx.) | ~9.8B, 7.4 versions each | ~1.0B, ~32 versions each |

  ZSTD already brings a raw storage entry of 92 bytes down to 18.6; the
  timestamp and sequence cost a few bytes after compression, not 16.
- **CPU.** RocksDB itself (index blocks, filters, iterators, timestamped key
  compares) is 40–50% of the CPU per request.
- **Maintenance.** Compaction is ongoing write amplification. The ZSTD rewrite
  took 12.9h (3.4TB read, 3.1TB written), and a truncating rewind trims the
  versioned CFs with a full compaction (5–10h, estimate).

Cold reads are already about one page each; a new format does not remove
them. What it can remove is the framing (size), the RocksDB code path (CPU)
and the compactions (maintenance). A smaller archive also leaves more of it in
the page cache.

## Constraints

From the sealed-segment attempt (#277, closed):

- No copy of the live state per epoch. #277 copied it at every segment start
  and grew to 3.6TiB against v2's 2.4TiB.
- Rewind must stay exact.
- Comparisons on the same EBS volume type and throughput as production.

## Model

Two parts:

- **Crown**: the latest value of every key (account, storage slot), with no
  versions. Mutable; stays in RocksDB. One copy for the whole chain.
- **History**: for every change of key `K` at block `t`, the record
  `(K, t, value of K before t)`. Append-only.

The value of `K` at block `N`:

1. Find `t`, the first change of `K` after `N`.
2. If there is one, the answer is the value before `t`: nothing changed `K`
   between `N` and `t`.
3. Otherwise the answer is the crown's value.

A key created at `t` stores "absent" as its value before `t`. History holds
one record per change, as many as v3 has versions, and the crown one value
per live key; nothing is copied per epoch.

Committing block `b` reads the crown's current value of each changed key
(point reads, mostly cached), appends `(K, b, old)` and writes the new value to
the crown.

## Rings

History is split by block range into **rings** of 2^20 blocks (about 9 days
on bsc):

- **Cambium**: the blocks not yet sealed, in a small mutable store (a RocksDB
  CF keyed `K ‖ t`). Lookups check it after the rings.
- **Sealing**: once a ring's blocks are final, a background job writes them
  into an immutable ring file and drops them from the cambium.
- **Ring file**:
  - keys sorted;
  - per key, its change blocks as deltas from the ring start (Elias-Fano or
    varints, ~1.5–2 bytes each), then its values-before;
  - packed into 4–16KB pages compressed with a per-ring ZSTD dictionary;
  - a page directory (first key of each page), whose top level stays in
    memory.

  A lookup in a ring reads one page.

**Which ring holds the first change after `N`.** The crown entry of `K` also
lists the rings in which `K` changed (Elias-Fano ring ids). It gains an entry
once per key per ring, when the key first changes in a new ring. A cold read
is then:

1. the crown entry (hot keys stay cached);
2. one page of the first ring at or after `N`'s ring that holds `K`.

If all of `K`'s changes in that ring are at or before `N`, move to the next
ring in the list, then to the cambium, then fall back to the crown value.

The alternative is a per-ring key filter in memory (~9 bits per distinct key
per ring), probing rings upward from `N`'s. Its memory grows with the history,
so it is only a fallback if the ring lists turn out too large.

## Rewind to `T`

Offline, in time proportional to the changes after `T`:

1. For each key changed after `T`, set its crown value to the value before
   its first change after `T`.
2. Delete the rings above `T`, rewrite the ring that contains `T` (at most
   2^20 blocks) and truncate the cambium.
3. Drop the removed ring ids from the crown's ring lists.

Block CFs are truncated as today.

## Migration from v3

v3's versioned CFs are sorted by key, then by block. One pass over them emits,
for each key with versions `v1 < v2 < …`, the change `(K, t_i, value at v_(i-1))`
into the writer of `t_i`'s ring, and the last value into the crown. Each ring
writer receives keys in order, so no sort is needed; the conversion is as
streaming as v2 → v3 was.

## What stays

- Block CFs (`BlockNumToBlockHash`, `BlockNumToBlockInfo` and its index),
  code and the latest pointer stay in RocksDB unchanged.
- The crate is `crates/leafage-rings`, in this repository. It shares the
  workspace's `librocksdb-sys` (Cargo links only one), and the converter and
  verify tools need v3 next to it.
- Its API knows only byte keys, `u32` blocks and byte values, and no leafage
  type. `leafage-evm-storage` implements `StateDBRead`/`StateDBWrite` on top of
  it. The crate can be split out later with `git subtree split`.

## Risks

The measured numbers show two problems with rings as described:

1. **Keys repeat per ring.** v3 keeps all versions of a key together and
   stores the key once. A ring stores again every key it changes (~33 bytes
   after prefix compression). A storage key has 7.4 versions on average; if
   they fall in 3–5 rings, each version pays 10–13 more bytes, more than v3.
2. **One more read for cold history.** Finding the ring of the first change
   after `N` reads the key's crown entry first. The crown then holds every key
   ever written (~10.8B) with its ring list, ~0.5TB (estimate), mostly not
   cached: two reads per cold lookup against one for v3, which is IOPS-bound at
   bsc peaks. #277 reached −51% CPU because each segment carried a state copy,
   so a read touched one segment and no RocksDB; without the copy that gain
   mostly goes.

What still looks worth it is the version encoding, with a key's versions kept
together: pack runs of versions into one record, blocks as deltas, account
values as changes from the previous version (nonce, balance; code hash once).
Estimate: storage ~14–16 bytes per version (key ~4.3 amortized, value ~8,
block ~2), account ~6–8; bsc 1.7TB → 1.3–1.5TB (−10–25%), reads per request
unchanged, CPU within ±10%.

## Measure first

On 14, with the bsc v3 archive, before writing the engine:

1. Versions per key, and in how many rings each key's versions fall
   (risk 1).
2. The bytes of a v3 storage version: key, timestamp and sequence, value,
   index, filter, after ZSTD.
3. The encoded size of a sample of keys in the ring format: change blocks as
   Elias-Fano deltas, values-before in ZSTD-dictionary pages.
4. The crown's size with ring lists.
5. A replay of production traffic against a prototype read path, on the same
   EBS: reads and CPU per request against v3.

Go ahead with rings only if they are at most 60% of v3 on disk, CPU per
request at most 70% of v3, and reads per request no more than v3. Otherwise,
measure the packed version encoding above instead.

## Non-goals

- MDBX archives and snapshot (non-archive) nodes.
- Changing how blocks, headers or code are stored.
