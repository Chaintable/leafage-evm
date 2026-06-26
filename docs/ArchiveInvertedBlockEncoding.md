# Archive Inverted Block-Height Key Encoding

This document describes the inverted (descending) block-height key encoding used
by the **Archive Node** storage, why it exists, and how it works. It complements
[Database.md](./Database.md), which describes the column-family layout.

> **Impact:** on a production Base archive node, switching from the ascending
> `seek_for_prev` layout to the descending forward-`seek` layout dropped
> `eth_call` latency from **~10 seconds to under 1 second**.

## TL;DR

Archive account/storage keys embed the block height so the node can answer
"state of this account/slot as of block `H`". Storing the height **inverted** —
`u64::MAX - block_num`, big-endian — makes versions of a key sort
**newest-first**, which turns the historical read from a backward `SeekForPrev`
into a forward `Seek`. Forward `Seek` is RocksDB's optimized iterator operation
*and* the only one that can properly use the per-CF prefix bloom filter. The net
effect is dramatically fewer SST reads per lookup, which matters enormously for
`eth_call`, where a single call can read hundreds of storage slots.

The encoding is selected at runtime by the **`--inverted-block-encoding`** flag
(on `standalone`, `archive-init`, and `db-migrate`). It is **off by default** —
existing deployments keep the legacy ascending layout and behaviour unchanged —
and must be matched between the DB and the command reading/writing it (see
[Enabling and migration](#enabling-and-migration)).

## Background: how the archive node stores state

An archive node must serve state at **any** historical height, so it keeps every
version of every account and storage slot, keyed by the block at which the
version was written (see [Database.md](./Database.md)):

```
AddressToAccount :  address(32) || block_num(32)              -> slim account (RLP)
AddressToStorage :  address(32) || slot(32) || block_num(32)  -> value (32, big-endian)
```

(The State Node, by contrast, stores a single flat value per key with no block
height, and is overwritten in place each block.)

A read at height `H` wants the **greatest version `<= H`** of a key. Because
RocksDB keeps keys in sorted order, all versions of one slot are physically
contiguous, ordered by the block-height bytes — so the read is a *seek* to the
right position within that contiguous run. The direction of that seek is the
entire subject of this document.

## A short RocksDB primer

Three facts drive the design:

1. **Keys are sorted; lookups are searches, not scans.** RocksDB orders keys by
   the `BytewiseComparator` (unsigned lexicographic byte order). A lookup binary-
   searches a sorted run — it does **not** walk from the start. Big-endian
   integer encoding is used precisely so byte order matches numeric order.

2. **Data lives in many sorted runs (LSM).** A key's versions are spread across
   the memtable, several L0 files, and one run per deeper level. Any read must
   consider every run that could hold the target and **merge** the candidates.
   Newly written (recent) versions live in shallow, hot levels; old versions
   sink to deep levels during compaction.

3. **Bloom filters decide which runs you touch.**
   - A **full-key bloom** answers "does this run contain this *exact* key?" It
     powers point `GET`: runs that answer "definitely not" are skipped, and the
     read short-circuits on the first (newest) hit.
   - A **prefix bloom** (built from a fixed-prefix extractor) answers "does this
     run contain *any* key with this prefix?" It is what lets a *seek* skip runs.

The archive account/storage CFs are configured with a fixed-prefix extractor and
prefix bloom — 32 bytes (`address`) for accounts, 64 bytes (`address || slot`)
for storage — so seeks can skip SSTs that don't hold the queried slot at all.

## The problem with ascending encoding + `SeekForPrev`

With ascending `block_num`, versions sort oldest→newest, so "greatest version
`<= H`" is the **largest key `<= address‖slot‖H`** — a backward `SeekForPrev`.
That is the wrong operation for two reasons.

### 1. `SeekForPrev` is the second-class operation

Forward `Seek` is RocksDB's primary, most-optimized iterator op. `SeekForPrev`
does more internal work, and reverse positioning over the multi-run merge heap
is inherently costlier.

### 2. The prefix bloom is effectively unusable on the backward path

This is the decisive issue, and it is worth stating carefully because the
asymmetry is subtle: it is **not** that a backward seek is "more likely to be
wrong" — it is about *which operation RocksDB exposes a prefix-scoped,
bloom-optimized variant for*.

#### The anchor: the only valid answer has prefix exactly `P`

The query is "newest version of *this* `(address, slot)`", so the prefix
`P = address(‖slot)` is **fixed and exact**. After the seek, the reader
prefix-checks the landed key and rejects anything whose prefix `≠ P`. A key with
a *different* prefix — whether smaller or larger than `P` — is therefore **never**
a valid answer. The prefix bloom answers exactly one question: "does this run
contain any key with prefix `P`?" A skip based on it is sound only if it can
never drop the run that holds our prefix-`P` answer.

#### Why the forward `Seek` skip is sound

Target `T = P‖(MAX − H)`; we want the smallest key `≥ T` **with prefix `P`**.
Take any run whose prefix bloom says "no key with prefix `P`". Can our answer be
in it? **No** — by definition it has zero prefix-`P` keys, and the answer must
have prefix `P`. So the skip can never lose the answer.

The natural objection: *"what if that skipped run holds a key with a prefix just
`≥ P` (a different, larger prefix `P''`) that is the smallest key `≥ T`?"* It
cannot matter, because of how sorting interacts with the prefix-check:

- Keys sort by full bytes, so **every** prefix-`P` key precedes **every**
  prefix-`P''` key (`P < P''`).
- If the slot *has* a version `≤ H`, that version is a prefix-`P` key `≥ T` and is
  **smaller** than any prefix-`P''` key, so it is the true answer — and it lives
  in a run the bloom did *not* skip. The `P''` key never wins.
- If the slot has *no* version `≤ H`, there is no prefix-`P` key `≥ T` at all and
  the correct answer is "absent". Whether we land on a `P''` key and reject it in
  the prefix-check, or skip its run and the iterator goes invalid, the result is
  identical: **absent**.

So a larger-prefix key `≥ T` is never something we want, and skipping its run
costs nothing. The forward skip never turns a real answer into a miss.

#### Why `SeekForPrev` cannot get the same skip

Symmetrically, *for our prefix-scoped query*, the largest prefix-`P` key `≤ T`
also cannot hide in a no-`P` run — so in principle a prefix-scoped *backward*
seek would be just as safe to optimize. The problem is that **RocksDB does not
offer a prefix-scoped backward seek.**

- **Forward `Seek` (in prefix-seek mode) *is* the prefix-scoped operation.**
  RocksDB knows the caller only wants keys sharing the target's prefix, so it is
  entitled to apply the prefix-bloom skip — that is the prefix bloom's entire
  purpose.
- **`SeekForPrev` is the *general total-order predecessor*:** "the largest key
  `≤ T` anywhere in the keyspace," whose answer **legitimately can be a smaller,
  different prefix** (the key just below `T` in a neighbouring prefix). RocksDB
  must honour that contract for every caller — including ones that do **not**
  prefix-check — so it cannot assume the answer has prefix `P`, and therefore
  cannot use "no key with prefix `P`" to skip a run: that run might hold the
  legitimate cross-prefix predecessor.

In short: the bloom skip is unsafe for the operation RocksDB actually exposes
(general predecessor), even though it would be safe for the prefix-scoped
predecessor we conceptually want. RocksDB only exposes a prefix-scoped, bloom-
optimized **successor** (forward `Seek`) — there is no prefix-scoped predecessor.

#### The consequence

So on the ascending layout, `SeekForPrev` runs in total-order mode (or requires
`total_order_seek`, which disables the prefix-bloom skip outright): the per-CF
prefix bloom is configured but **half-bypassed**, and every backward lookup must
position in and merge across every run that could hold a key near `T` — not just
the runs that actually hold prefix `P`. Inverting the height converts our
"newest `≤ H`" into a forward `Seek`, the one operation that gets the skip.

### Why this made `eth_call` slow

An `eth_call` reads one account but often **hundreds of storage slots**. The
in-memory diff layers only hold slots changed in the last ~64 blocks, and the
moka cache holds only hot slots, so a cold/large call falls through to disk for
most slots. On the ascending layout, every such slot was a `SeekForPrev` that
could not use the prefix bloom and merged across many runs — hundreds of those
per call is how a single `eth_call` reached ~10 seconds.

## The solution: invert the height, seek forward

Store the height as `MAX - block_num` (big-endian) in the key tail. Now versions
of a key sort **newest-first**, and "greatest version `<= H`" becomes the
**smallest key `>= address‖slot‖(MAX - H)`** — a plain forward `Seek`.

Correctness rests on the identity:

```
(MAX - bn) >= (MAX - H)   <=>   bn <= H
```

The versions with `bn <= H` are exactly those whose stored tail is `>= (MAX - H)`,
and the *smallest* such tail is the *largest* `bn <= H` — which is what
"smallest key `>= target`" returns.

### Worked example

Slot `S` of address `A` was written at blocks **5, 10, 20**. Query at `H = 15`.

Ascending tails sort `5 < 10 < 20`; you `SeekForPrev` to `…‖15` → block 10.

Inverted tails (`MAX - bn`):

| block | stored tail | sorts as |
| ----- | ----------- | -------- |
| 20    | `MAX-20`    | 1st (smallest) |
| 10    | `MAX-10`    | 2nd |
| 5     | `MAX-5`     | 3rd (largest) |

Target tail is `MAX-15`. Forward `Seek` finds the smallest tail `>= MAX-15`:

```
MAX-20  <  MAX-15 (target)  <=  MAX-10  <  MAX-5
                                ^ first key >= target  => block 10  ✓
```

A query at the tip (`H = latest`) lands on the very first entry of the prefix
(the newest version) — the cheapest possible seek position.

### Edge cases (handled identically by both backends)

- **No version `<= H`** (slot first written after `H`): all of this slot's tails
  are `< (MAX - H)`, so the forward seek steps past them into the next prefix (or
  the iterator becomes invalid). The read then prefix-checks the landed key; a
  mismatch means the slot was absent at `H` → returns `None`/`ZERO`.
- **Deletion**: a deletion is stored as an empty value at its block. If the
  newest version `<= H` is a deletion, the read returns absent.

### Why forward `Seek` now wins

It is the optimized RocksDB op, and — crucially — the prefix bloom is now sound:
"smallest key `>= target` within prefix `P`" means a run with no key of prefix
`P` holds nothing relevant, so the bloom's "no" is a valid skip. The
already-configured 32-byte / 64-byte prefix blooms finally do their job, so each
lookup touches only the runs that actually hold the queried slot.

## Implementation

All changes are in `crates/leafage-evm-storage/src/db_impl/` and affect **both**
archive backends (RocksDB and MDBX), kept consistent because the key encoding is
shared. The mode is a process-global selected once at startup, so reads, writes,
and iterators all agree for the lifetime of the (single-per-process) archive DB.

### Mode selection (`archive_encoding.rs`)

- `set_inverted_block_encoding(bool)` / `inverted_block_encoding() -> bool` — a
  process-global `AtomicBool` (default `false`). The CLI sets it once at the top
  of `run()` before any key is encoded or any DB opened.
- `encode_block_num(bn)` — raw **ascending** big-endian. Still used unconditionally
  by the `BlockNumToBlockHash` index, whose readers decode the raw block number.
- `encode_block_num_desc(bn) = encode_block_num(u64::MAX - bn)` — descending.
- `encode_account_key` / `encode_storage_key` — pick the tail per
  `inverted_block_encoding()`: descending when set, ascending when not.

The module doc carries the full rationale (this document is the expanded form).

### Reads

`read_account` / `read_storage` branch on `inverted_block_encoding()`:

- **inverted** → forward seek (RocksDB `iter.seek`, MDBX `set_range` / `seek_ge`).
- **legacy** → backward seek (RocksDB `iter.seek_for_prev`, MDBX `seek_le`).

Both keep the existing post-seek prefix check, which handles the "no version
`<= H`" case in either mode.

### Full-scan iterators

The `LatestStateDBIterator` (`account_iter` / `storage_iter`) reconstructs the
latest state by scanning. The newest version is the **last** record of each
prefix under ascending keys and the **first** under descending keys, so both
backends branch on the mode (peeking the next record for legacy last-per-prefix,
tracking the consumed prefix for inverted first-per-prefix), skipping the whole
prefix when the newest version is a deletion / zero.

### Unaffected

- `BlockNumToBlockHash`, `BlockHashToBlockInfo`, `HashToCode`,
  `LatestBlockHash` — no version tail, untouched.
- The **State Node** (snapshot) storage — it has no versioned keys at all.
- The in-memory diff tree / `CacheDiskLayer` / `--diff-depth-limit` window — the
  change is purely about the bottom on-disk layer's key order.

### `archive-init`

`archive-init` builds keys through the same `encode_account_key` /
`encode_storage_key`, so it produces the inverted format automatically. Its SST
ingest pipeline sorts by the **encoded** key bytes, which remain strictly
increasing under the new encoding, so the bulk-load path is unaffected.
`archive-init` is therefore the tool used to (re)build archive DBs in the new
format.

## Enabling and migration

The encoding is opt-in via **`--inverted-block-encoding`**, off by default, so
upgrading the binary alone changes nothing — existing archive DBs keep the
ascending layout and are served with the legacy backward-seek readers.

The two layouts are **mutually unreadable** for the `AddressToAccount` /
`AddressToStorage` CFs: keys written ascending are not correctly read by the
forward-seek readers and vice versa. There is **no in-DB format marker**, so the
operator is responsible for keeping the flag consistent — a mismatch fails
*silently* (wrong values), not loudly.

To adopt the inverted layout:

1. **Rebuild** the archive DB with `archive-init --inverted-block-encoding`
   (or re-sync into a fresh DB with the flag).
2. **Serve** it with `standalone --archive --inverted-block-encoding`.

The flag on the builder and on the serving node must match the bytes on disk.
The default (no flag, ascending) and a fully-rebuilt inverted DB are both
self-consistent; the only failure mode is pointing a flag at a DB written with
the other setting. State Node databases are unaffected by the flag and require
no migration.

`db-migrate` reads an archive source through the same latest-state iterators, so
it carries the flag too: migrating an **inverted** archive into a snapshot needs
`--src-is-archive --inverted-block-encoding`. Omitting it reads the archive as
ascending — the iterators pick the *wrong* version per key and silently produce a
corrupt snapshot.

## Results

On a production Base (chain 8453) archive node, `eth_call` latency dropped from
**~10 seconds to under 1 second** after adopting the inverted encoding — driven
by each storage-slot lookup becoming a forward `Seek` that uses the prefix bloom
instead of a `SeekForPrev` that merged across many runs.

## Limitations and future work

The inverted encoding optimizes the **seek**; it does not turn latest reads into
point lookups. Two complementary follow-ups:

1. **Flat latest-state CFs (point GET for the tip).** Maintain version-free
   `LatestAccount` / `LatestStorage` CFs (overwritten each block) alongside the
   versioned history CFs. Reads at `latest` (and within the diff window) become
   a point `GET` — identical cost to a State Node, with **zero** version-search —
   while deep-historical reads keep the forward-seek path. This is the
   single-biggest additional win for tip-dominated traffic; the cost is one
   extra overwrite per changed entry per block and roughly one live-state-sized
   copy on disk.
2. **Changesets + inverted bitmap index** (Erigon-style) as a more scalable
   history representation, bounding historical-read cost independent of version
   count, if archive size / deep-query volume ever demands it.
3. **In-DB format-version marker** so a pre-inversion DB fails loudly on open
   instead of returning wrong data.

## References

- Code: `crates/leafage-evm-storage/src/db_impl/archive_encoding.rs`,
  `.../rocksdb_impl/archive/mod.rs`, `.../mdbx_impl/archive/mod.rs`
- Column-family layout: [Database.md](./Database.md)
- State management / diff tree: [StateManage.md](./StateManage.md)
