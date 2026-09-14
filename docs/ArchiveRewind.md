# Archive rewind

`rewind --archive` **deletes future archive state and block indexes in place** by
default. Stop every database user before running it:

```sh
leafage-evm rewind --archive --db-path /path/to/archive --to-block 123456
```

To retain future versions and only move the committed head, explicitly use
`--head-only`. This preserves the previous archive rewind behavior and accepts
`--keep-offset`:

```sh
leafage-evm rewind --archive --head-only \
  --db-path /path/to/archive --to-block 123456
```

Head-only rewind does not repair fork contamination: if X@100=5 and old-branch
X@101=9, replaying a new block 101 that does not write X exposes the old value 9.
Default truncation removes that old version before replay.

Snapshot/state rewind retains its existing pointer-only behavior. It cannot remove
stale fork state; regenerate a polluted state database from a repaired archive.

Both RocksDB (default) and MDBX (`--db-type mdbx`) are supported. Stop every database
user, including initialization, migration, compaction and read-only export, and
prevent automatic restart during truncation. RocksDB holds its writer lock;
MDBX maintenance opens exclusively. A RocksDB writer lock does not exclude
read-only opens. Deleted history requires a backup or resync to recover.

## Inputs and prechecks

- `--head-only` requires `--archive`. Default archive truncation rejects
  `--keep-offset` before opening the database; snapshot rewind still accepts it.
- `--inverted-block-encoding` uses the same flag name and default as `standalone`
  and `archive-init`: unmarked archives use legacy keys unless the flag is passed.
  Pass it for unmarked inverted archives, including inverted MDBX. A valid RocksDB
  encoding marker determines the encoding when the flag is absent; truncation
  rejects an explicit inverted flag conflicting with a legacy marker. Malformed
  markers and read errors stop truncation. Key bytes cannot reliably identify the
  encoding, so an unmarked archive still requires a correct operator choice.
  Head-only opens retain the normal reader's marker precedence.
- The PR's earlier `--truncate-archive` and `--archive-encoding` options are removed.
- Offset paths use the existing rewind rule: a nonempty `offset_dir` in
  `--kafka-s3-config` selects `<offset_dir>/offset`; otherwise use
  `<db-path>/offset/offset`. Kafka configuration is optional. A missing offset
  file is accepted; no additional offset flags or source declaration are required.
- Only existing databases and tables are opened. Account/storage key shapes must
  identify an archive. Empty, unmarked databases cannot establish this and are
  rejected. Every scanned key is also validated; sampling does not prove that a
  database contains no mixed or corrupt records.
- The target number, hash and header must agree, with `H <= committed head C`.
  `H = C` still scans and removes future data left by an earlier pointer rewind.
  Physical records above C are included; they are not assumed invalid.

Precheck failure leaves state and offset untouched. Correct input encoding,
complete and correct history through H, and downtime are preconditions. Rewind
does not repair incorrect history already at or below H.

## Changes and correctness

The command deletes account/storage versions above H, number-to-hash entries above
H, and headers whose number is above H (including orphan hashes). It removes legacy
latest-pointer sentinels, preserving inverted height zero. All versions at or below
H, including deletion tombstones and zero storage values, remain unchanged.
Content-addressed code and format metadata are retained. Unreferenced future code
may remain visible through raw code iteration. Memory views are rebuilt on restart;
the optional token collector file is only a warm-up address list and is retained.

For any key and `h <= H`, the set of versions eligible for a historical read is
unchanged, so the result is unchanged. After all versions above H are removed,
new-branch writes either supply a new value or inherit the preceding value; old
future values cannot reappear. Keeping code referenced by retained accounts and
removing future block indexes completes the state and block-query guarantee.
This is semantic equivalence, not a byte-for-byte copy of an earlier database.

Both backends perform a full scan. RocksDB uses total-order iterators with checksum
verification and synchronous WAL writes. MDBX releases each bounded read transaction
before its deletion transaction and resumes strictly after the last inspected key.
Batches flush at 10,000 entries or 1 MiB: RocksDB counts serialized deletion bytes;
MDBX counts scanned key/value bytes. A single record may cross the byte threshold.
RocksDB iterators can retain old SST files until a table scan finishes.

## Execution order and failure behavior

1. Validate inputs, encoding, archive layout and the target block.
2. Remove the actual offset file and fsync its directory (or the nearest existing
   ancestor if the directory is absent).
3. Delete future records in durable, bounded batches. Errors stop processing.
4. Publish the target head durably after every table has completed.

There is no rewind recovery marker, checkpoint or dedicated interruption recovery.
Normal startup, initialization, migration and compaction have no new rewind guards;
shared open/header-decoding code preserves their existing behavior. The existing
archive encoding marker is unrelated and remains supported.

Each batch is atomic; the entire truncation is not. An error before deletion leaves
archive records unchanged. After deletion starts, an error can leave partial
truncation, including a missing old-head header, so repeating the command is not
guaranteed to work. Only successful completion establishes the result described
above. Keep database users stopped if truncation fails; normal opens do not detect
an unfinished truncation.

After completion, restart using the intended branch and the existing no-offset
catch-up path. The upstream must retain the blocks needed to continue from H.

## Verification and cost

Local validation on 2026-09-14: all 40 storage tests passed (3.12 s), and all four
rewind CLI tests passed (0.72 s). The CLI database test covers six combinations:
MDBX legacy/inverted, unmarked RocksDB legacy/inverted, and marked RocksDB
legacy/inverted. It verifies head-only retention and keep-offset, default same-height
truncation, retained balances, marker auto-detection and conflict rejection. Other
CLI tests cover parameter constraints, offset paths, and rejecting keep-offset
before opening an archive for truncation. Modified Rust files pass rustfmt checks.

The storage algorithm at `17e2c1c` was tested on lihe-dev-next on 2026-09-11/12
using the same candidate image and independent snapshot copies:

| Case | Truncation | State comparisons | Limits |
| --- | --- | --- | --- |
| CRO RocksDB / inverted | 1,453.570 s; 1,475,263 block-number entries and 1,486,159 headers removed | 444/444 after truncation; 759/759 after replay and after restart | Existing estimateGas difference 24329 vs 23991 remains an exact mismatch |
| HSK RocksDB / legacy, initially unmarked | 272.026 s; 2,048 block-number entries and 2,048 headers removed | 223/223 after truncation; full replay retry 406/406; restart 402/406 plus the four timed-out assertions rechecked successfully | 82 physical sample keys: 82 before, 41 retained, 82 byte-identical after replay; no legacy sentinel in this sample |

These runs used the earlier explicit truncation CLI. They validate that revision's
storage behavior; the default/flag changes in this revision have not been deployed.
HSK's original failed requests were retained; its restart result is not a single
406/406 run. Both real databases were RocksDB, so MDBX and legacy sentinel handling
have local unit-test coverage only. RPC and physical-key samples are not a full
key-by-key comparison of the database. Physical power-loss behavior was not tested.

CRO replay completed 10,960 blocks across archive-init checkpoints after forwarding
failures. The final 3,625 blocks took 711.8 s at concurrency 8, followed by 2,399.540 s
of compaction. HSK replayed 2,048 blocks in 344.3 s, followed by 561.284 s of
compaction. These compaction timings belong to replay, not the rewind command.

Work scales with the entire archive. Reserve WAL/compaction space; immediate file
shrinkage and code garbage collection are not completion criteria. Tests verify
the implementation against the stated invariants, rather than establish those
invariants by themselves.
