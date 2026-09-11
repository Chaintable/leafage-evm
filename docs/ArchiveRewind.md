# Archive rewind

`rewind` keeps its existing behavior: it moves the committed head and applies the
existing offset policy. This also applies to `rewind --archive`; future versions
remain in the database, and `--keep-offset` is still accepted.

To **delete future archive state in place**, explicitly add `--truncate-archive`:

```sh
leafage-evm rewind --archive --truncate-archive \
  --db-path /path/to/archive --to-block 123456 \
  --archive-encoding legacy
```

Both RocksDB (default) and MDBX (`--db-type mdbx`) are supported. Stop every database
user, including initialization, migration, compaction and read-only export, and
prevent automatic restart during truncation. RocksDB holds its writer lock;
MDBX maintenance opens exclusively. A RocksDB writer lock does not exclude
read-only opens. Deleted history requires a backup or resync to recover.

## Inputs and prechecks

- `--truncate-archive` requires `--archive` and conflicts with `--keep-offset`.
- A valid RocksDB encoding marker determines the encoding. An explicit
  `--archive-encoding legacy|inverted` must agree with it. Malformed markers and
  read errors stop the operation. Unmarked archives, including MDBX, require an
  explicit, trusted encoding; the key bytes cannot reliably identify it.
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

Local validation on 2026-09-11 after reusing the existing offset paths: 40 storage tests passed
(3.17 s), and 3 rewind CLI tests passed (1.17 s). Coverage includes both
backends/encodings, tombstones, orphan headers, same-height cleanup, new-branch
reads, malformed records stopping before head publication, encoding conflicts,
offset reset failure and header compatibility. The CLI test checks that default
archive rewind preserves future versions and `--keep-offset`, then explicit
truncation removes them.

The earlier CRO deployment used commit `2d4893a`, which still had recovery. After an
intentional SIGKILL at 165 seconds, that version rescanned and completed in about
23 minutes. Replay of 10,960 blocks took about 110 seconds; state comparisons passed
444/444 after truncation and 759/759 after replay and restart. These are historical
measurements, not a deployment or benchmark of the current revision. Physical
power-loss behavior has not been tested.

Work scales with the entire archive. Reserve WAL/compaction space; immediate file
shrinkage and code garbage collection are not completion criteria. Tests verify
the implementation against the stated invariants, rather than establish those
invariants by themselves.
