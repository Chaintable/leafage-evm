# Archive rewind

`rewind --archive` truncates a local Archive database to a block height. Stop the
node before running it. Both RocksDB and MDBX archives are supported.

```sh
leafage-evm rewind --archive --db-path /path/to/archive --to-block 123456
```

For MDBX, add `--db-type mdbx`. RocksDB uses its database lock; MDBX opens the
environment exclusively for rewind. Preserve a backup if the discarded history
may be needed later.

The command removes all account and storage versions above the target, their
block-number mappings, and headers above the target (including orphan hashes).
Deletion tombstones and zero storage values at or below the target are preserved.
Legacy dual-write latest-pointer entries are removed. Content-addressed bytecode
is retained, including unreferenced code; rewind does not garbage-collect code.

After all tables have been truncated, the command publishes the target head.
Historical reads at or below the target remain available. Future block numbers
and removed hashes are no longer queryable, and continuing on a different branch
cannot expose the discarded account/storage versions.

## Encoding

A RocksDB encoding marker takes precedence over the CLI setting. For an unmarked
inverted archive, or an inverted MDBX archive, specify
`--inverted-block-encoding`. Without a marker, this flag must match the source
database. The number-to-hash index always uses ascending heights.

## Restart and interrupted operations

Before deleting state, the command removes and syncs the Kafka offset file. A
custom `offset_dir` requires the same `--kafka-s3-config` used by the node;
otherwise the default is `<db-path>/offset/offset`. `--keep-offset` is rejected
in archive mode. Restart `standalone` to catch up from the rewound state.

A persistent database marker records the target and encoding before the first
deletion. If rewind fails or the process stops, normal `MultiStorage::open`
(including `standalone`) refuses to open the unfinished archive. Rerun with the
same target, encoding and offset configuration. Recovery rescans from the start;
already committed deletions are harmless. A malformed key/header aborts the
operation and leaves the marker present; repair or restore the database before
retrying. Do not manually remove the marker.

The target may equal the current head, allowing retries and cleanup after the
previous pointer-only rewind implementation. A rewind cannot repair incorrect
state at or before its target: choose a known-good common ancestor or rebuild.

## Cost and scope

The implementation scans the versioned account/storage tables and block indexes,
using bounded deletion batches. Work scales with stored history, not just the
number of blocks rewound. RocksDB deletion makes rows invisible immediately after
commit; physical disk space is reclaimed by subsequent compaction.

Snapshot/State mode keeps its existing pointer-reset behavior. It does not gain
historical rollback or undo logs from this change.

## Validation

Local validation on macOS:

- `cargo test -p leafage-evm-storage --lib --offline -- --test-threads=1`: 36 passed
  after integration with main `a940382`, including both backends/encodings and
  reopening interrupted rewinds (4.01 s).
- `cargo test -p leafage-evm rewind::tests --locked -- --test-threads=1`: 2 passed,
  covering default/custom offsets and rejection of `--keep-offset` (0.01 s).
- `cargo check -p leafage-evm --offline` and formatting/diff checks passed.

These are fixture-level correctness checks; production-size scan throughput,
disk-space reclamation and power-loss behavior have not been benchmarked.
