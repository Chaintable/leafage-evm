# Archive schema v3: sealed history segments with point lookups

Status: **draft / prototype**. This document defines the semantics of archive
schema v3 and the on-disk format of its sealed history segments. The
prototype implements the read path and an offline builder; sealing new
epochs on a live node is specified here but not yet implemented.

## Schema versions

| Version | Account / storage history                                            | Marker                                      |
|---------|----------------------------------------------------------------------|---------------------------------------------|
| v1      | RocksDB CFs `4`/`5`, key `key ‖ block` (32-byte ascending tail), read with `SeekForPrev`; may still carry pre-#104 `u64::MAX` dual-write rows | none, or `leafage:block_encoding_inverted = 0` |
| v2      | Same CFs, 32-byte **descending** tail `u64::MAX - block`, read with a forward `Seek` (#162, see [ArchiveInvertedBlockEncoding.md](./ArchiveInvertedBlockEncoding.md)) | `leafage:block_encoding_inverted = 1` |
| v3      | Blocks `≤ S` in immutable **sealed segments** (this document); blocks `> S` in the v2 CFs (the *tail*) | `leafage:archive_schema = 3` + `history/MANIFEST.json` |

Block info, the block-number index, the latest pointer and code stay in
RocksDB in every version; v3 only moves account and storage history.

## Logical model (unchanged from v1/v2)

Every schema version stores the same logical data and must answer reads
identically:

1. **Keys.** An account key is a 32-byte address key; a storage key is the
   64-byte `address key ‖ slot key`. Keys are opaque bytes to the archive.
2. **Versions.** History is a set of versions `(key, block, value)`, at most
   one per `(key, block)` (the last write of a block wins).
3. **Read.** `read(key, h)` is the value of the version with the greatest
   `block ≤ h`. No such version: an account is absent (`None`), a storage slot
   is zero.
4. **Values.** An account value is `RLP(SlimAccount)`; an empty value means
   the account was deleted at that block. A storage value is a 256-bit
   integer; zero means unset.
5. **Append-only.** The archive only appends final blocks above its head;
   history at or below the head never changes. Reorgs stay in the in-memory
   diff layers; rewind is an offline command.
6. **No incarnations.** Deleting an account (`SELFDESTRUCT`, EIP-161 cleanup)
   writes an explicit zero version for every live slot of it, so a read never
   needs to know whether a slot belongs to an older incarnation.

`read_storage_and_height` also returns the block of the version it read.
v3 relaxes this one point: for a **zero** value written before the epoch
that serves the read, the height may be reported as `None` (no version).
The value is unaffected. The only consumer of the height outside the cache
is `archive-clear-deleted`, an offline v2 repair tool.

## Layout

```
<db>/                        RocksDB (block info, index, code, tail versions > S)
<history>/MANIFEST.json      sealed boundary S, epochs, files
<history>/epoch-<start>-<end>/account-<shard>.seg
<history>/epoch-<start>-<end>/storage-<shard>.seg
```

`<history>` defaults to `<db>/history`.

### Epochs

Sealed history is a list of contiguous **epochs** `E_0 … E_n`,
`E_i = [start_i, end_i]`, `start_0 = 0`, `start_{i+1} = end_i + 1`, and the
**sealed boundary** `S = end_n`. Epoch `i` holds, for each key:

- a **base version**: the greatest version with `block < start_i`, kept only
  if it is live (an existing account, a non-zero slot), with its original
  block number; and
- every version with `block ∈ [start_i, end_i]`.

So for any `h ∈ E_i`, `read(key, h)` is answered by epoch `i` alone. A read
touches exactly one segment, whatever the depth of the history: that is the
point of v3. The price is one copy of the live state per epoch; cutting a
new epoch only once the versions written since the last cut reach `k` times
the live-state size bounds total size by `(1 + 1/k) ×` the history.

### Tail

Versions with `block > S` live in the v2 CFs. They may also still hold
versions `≤ S` that were sealed but not yet garbage-collected; those are
exact copies of sealed history and are ignored.

### Read algorithm

```
read(key, h):
  if h > S:
      v = tail.seek(key, h)              # greatest version <= h, v2 forward seek
      if v exists and v.block > S: return v
      h = S                              # nothing above S: the value at S holds
  i = epoch containing h                 # binary search over the manifest
  return epoch_i.lookup(key, h)          # greatest record <= (key, h), or none
```

The state cache keeps spans as before. An epoch lookup gives the exact span
for free: the key's next record in the segment ends it, else `end_i` does.
A lookup that finds nothing caches `[start_i, end_i]` (not `[0, …]`): an
earlier epoch may hold a value.

### Sharding

Each epoch is split into `2^b` shards by the top `b` bits of the key's first
byte (`b` is in the manifest; the prototype uses 4). Shards are independent
files, built in parallel.

## Segment file format (`.seg`, version 1)

Little-endian unless noted. The file is a sequence of 4096-byte pages:
page 0 is the header, then the data pages, then the fence levels from the
bottom up.

### Records

A record is `(key, block, value)`. Records are sorted by key, then by
**ascending** block. The record key is `key ‖ block` with the block as
8-byte **big-endian**, so bytewise order equals record order. Compared with
v2:

- the version tail is the 8-byte block, not 32 bytes;
- storage values are big-endian with **leading zeros stripped** (zero is the
  empty string); account values are `RLP(SlimAccount)`, empty when deleted;
- no inversion: a point lookup does not care about seek direction.

### Data page

```
u16 n_keys
u16 n_records
u32 reserved (0)
[u8; K]  keys[n_keys]          distinct keys of the page, ascending
u16      first[n_keys]          index of each key's first record
u64      block[n_records]       per key ascending
u16      value_end[n_records]   end offset of each value in the page
...      value bytes            value i spans [value_end[i-1], value_end[i])
                                (value_end[-1] = start of the value area)
```

`K` is 32 (account) or 64 (storage). A record never spans pages; a key's
versions may continue on the next pages, each page repeating the key once.

### Fence levels

Level 0 has one entry per data page, level `j + 1` one entry per level-`j`
page; the top level is a single page. Entry `p` of level 0 is a separator
`s_p` with `last(p - 1) < s_p ≤ first(p)` over record keys (`s_0` is empty):
the shortest prefix of `first(p)` greater than `last(p - 1)`. For hashed
keys that is a few bytes, except inside one key's long run of versions.
Entry `p` of level `j + 1` is the first entry of level-`j` page `p`.

```
u64 first_entry                 level-wide index of the page's first entry
u16 n_entries
u16 entry_end[n_entries]        end offset of each separator in the page
...  separator bytes
```

A lookup of `(key, h)` builds the target `key ‖ be64(h)`, finds in each level
from the top the greatest entry `≤ target` (bytewise, a proper prefix sorts
first), descends to the data page, and takes the greatest record
`≤ target` there. If that record has `key`, it is the answer and the next
record (in the page, else the first one of the next page) bounds its span;
otherwise the key has no version `≤ h` in this epoch. Either way the
record after that position, if it has `key`, is the key's next version: a
miss below a key's first version must still report it, or the cache would
take the key as absent for the whole epoch.

### Header (page 0)

```
[u8; 8]  magic "LFV3SEG\0"
u32      format version (1)
u8       kind (1 = account, 2 = storage)
u8       key length K
u16      shard
u64      epoch start, epoch end
u64      records, keys, data pages
u8       fence levels L (≤ 8)
L ×      (u64 first page, u64 pages, u64 entries)
u32      CRC32 of the preceding header bytes
```

### Manifest

`history/MANIFEST.json`, replaced atomically (write a temporary file,
`fsync`, `rename`, `fsync` the directory):

```json
{
  "schema": 3,
  "sealed_upto": 128538186,
  "shard_bits": 4,
  "epochs": [
    {"start": 0, "end": 128538186, "dir": "epoch-0-128538186",
     "account": [{"file": "account-00.seg", "bytes": 0, "records": 0}],
     "storage": [{"file": "storage-00.seg", "bytes": 0, "records": 0}]}
  ]
}
```

## Building and sealing

- **Initial build** (`archive-v3-build`, implemented): read a v2 archive
  read-only and write epoch 0 covering `[0, head]`, so `S = head`; the
  RocksDB CFs are left untouched. A node then serves `≤ S` from the
  segments and appends new blocks to the tail as today.
- **Sealing** (specified, not implemented): build epoch `n + 1` from the
  tail versions in `(S, S']` plus the live state at `S` (the last live record
  per key of epoch `n`, a streaming merge), publish the manifest, and only
  then let a compaction filter drop tail versions `≤ S'`.

## Compatibility and limits

- A v3 database must only be opened by v3-aware binaries: once sealed
  versions are garbage-collected from the tail, an older binary would read
  the tail alone and silently return stale state.
- Rewind below `S` drops whole epochs; the prototype only supports rewinding
  within the tail.
- Full latest-state scans over segments (`db-migrate` from v3, `archive-scan`)
  are not implemented in the prototype.
