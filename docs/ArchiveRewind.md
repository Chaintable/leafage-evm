# Archive rewind

`rewind` keeps its existing behavior: it moves the committed head and applies the
existing offset policy. This also applies to `rewind --archive`; future versions
remain in the database, and `--keep-offset` is still accepted.

To **delete future archive state in place**, explicitly add `--truncate-archive`:

```sh
leafage-evm rewind --archive --truncate-archive \
  --db-path /path/to/archive --to-block 123456 \
  --archive-encoding legacy --offset-dir /path/to/node-offset
```

Both RocksDB (default) and MDBX (`--db-type mdbx`) are supported. Stop every database
user, including initialization, migration, compaction and read-only export, and
prevent automatic restart with an older binary. RocksDB holds its writer lock;
MDBX maintenance opens exclusively. A RocksDB writer lock does not exclude
read-only opens. Deleted history requires a backup or resync to recover.

## Inputs and prechecks

- `--truncate-archive` requires `--archive` and conflicts with `--keep-offset`.
- A valid RocksDB encoding marker determines the encoding. An explicit
  `--archive-encoding legacy|inverted` must agree with it. Malformed markers and
  read errors stop the operation. Unmarked archives, including MDBX, require an
  explicit, trusted encoding; the key bytes cannot reliably identify it.
- Provide exactly one offset source: `--kafka-s3-config`, `--offset-dir`, or
  `--no-kafka`. The last option explicitly declares that the node does not consume
  Kafka. A config with no custom offset directory resolves to
  `<db-path>/offset/offset`; `--offset-dir DIR` resolves to `DIR/offset`.
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

## Durability and recovery

The storage API enforces this order:

1. Validate inputs and durably record marker version, original head, full target
   block, encoding and absolute offset path/source (or no Kafka).
2. Remove the actual offset file and fsync its directory. A retry with an absent
   file still syncs the directory, or its nearest existing ancestor.
3. Delete future records in durable batches. Errors stop processing.
4. Atomically publish the target head and remove the marker.

Each intermediate batch only removes records that must disappear. Retained history
is unchanged, and repeating the scan is idempotent. The marker remains until every
table is complete, including if offset reset fails. Normal startup, pointer rewind,
archive initialization, migration/re-encoding and compaction reject a pending
marker. Re-encoding rejects its source before creating the destination.

After interruption, repeat the command with the same target, encoding and offset
source/path. It rescans from the start and does not require the old head's header,
which may already have been deleted. Unknown marker versions fail closed. Do not
remove the marker manually or use an older binary on an unfinished database.
Successful retries at H=C still undergo format checks; an empty unmarked result
cannot subsequently be identified as archive without trusted format metadata.

After completion, restart using the intended branch and the existing no-offset
catch-up path. The upstream must retain the blocks needed to continue from H.

## Verification and cost

Local validation on 2026-09-10: 40 storage tests passed (2.88 s), and 3 rewind CLI
tests passed (0.20 s). Coverage includes both backends/encodings, tombstones, orphan
headers, same-height cleanup, new-branch reads, partial deletion/reopen, loss of the
old head header, offset reset failure, option/marker conflicts, header compatibility
and normal-entry guards. The CLI test verifies that default archive rewind retains
future versions and `--keep-offset`, then explicit truncation removes them.

These tests check the implementation against the stated invariants. Physical power
loss, a production-sized archive, full RPC/EVM state comparison and throughput/RSS/
disk measurements have not been performed. Work scales with the entire archive;
reserve WAL/compaction space. Immediate file shrinkage and code garbage collection
are not completion criteria.
