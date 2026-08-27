# State Updater

This document describes how leafage-evm receives and processes state updates.

## Overview

leafage-evm does not perform P2P synchronization. Instead, it receives state updates through two modes:

| Mode | Primary Use | Data Source |
|------|-------------|-------------|
| Kafka + S3 | Production | Kafka notifications + S3 block data |
| HTTP | Development/Fallback | Geth RPC (`trace_debankBlock`) |

## Mode Selection

The updater mode is selected based on CLI parameters:

```
┌─────────────────────────────────────────────────────────────┐
│                    updater_build()                          │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  kafka_s3_config provided?                                  │
│       │                                                     │
│       ├── Yes ──► KafkaUpdater (primary)                    │
│       │                                                     │
│       └── No ──► rpc_addr provided?                         │
│                      │                                      │
│                      ├── Yes ──► HttpUpdater (fallback)     │
│                      │                                      │
│                      └── No ──► No updater (static state)   │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

## Kafka + S3 Mode (Primary)

### Architecture

```
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│     Kafka       │     │       S3        │     │   leafage-evm   │
│                 │     │                 │     │                 │
│  Block change   │     │  - Block info   │     │  KafkaUpdater   │
│  notifications  │────►│  - State diffs  │────►│                 │
│                 │     │                 │     │  StateTree      │
└─────────────────┘     └─────────────────┘     └─────────────────┘
```

### Update Flow

```
┌─────────────────────────────────────────────────────────────┐
│                    Kafka Message Flow                        │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  1. Receive KafkaBlockChangeNotification                    │
│     └── Contains: new_blocks[] with block hash, parent hash │
│                                                             │
│  2. Fetch block info from S3 (parallel)                     │
│     └── s3://{bucket}/{chain_id}/[{ver}/]{hash}/block       │
│                                                             │
│  3. Fetch state diff from S3 (parallel)                     │
│     └── s3://{bucket}/{chain_id}/[{ver}/]{root}/stateDiff   │
│     └── Skip if state_root unchanged (empty diff)           │
│     └── Chain 999: {hash}/stateDiff, always fetched         │
│                                                             │
│  4. Apply updates to StateTree                              │
│     └── tree.update_block(block_info, block_diff)           │
│                                                             │
│  5. Commit offset after persistence                         │
│     └── write_offset(offset_dir, offset + 1)                │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

S3 keys are `{chain_id}/[{version}/]{block_hash}/block` for block info and
`{chain_id}/[{version}/]{state_root}/stateDiff` for the state diff. The
`{version}` segment is only present when `version` is set in the config.
State diffs are keyed by state root, so blocks that leave the root unchanged
share one object and are skipped rather than fetched.

HyperEVM (chain `999`) is the exception: it reports a zero state root on every
block, so its diffs are keyed by `{block_hash}` and fetched for every block.
See `docs/DataSpec.md` for why, and `state_diff_keyed_by_block_hash()` in
`bin/leafage-evm/src/utils.rs` for the gate.

### Offset Management

KafkaUpdater maintains Kafka consumer offset for crash recovery:

```
Startup:
  1. Read persisted offset from offset_dir
  2. Fetch Kafka watermarks (lowest, latest)
  3. Decision:
     ├── bundle_bucket_name set ──► Sync from bundle/block storage, start from latest
     ├── offset >= lowest ──► Resume from offset
     └── offset < lowest or missing ──► Sync from S3, start from latest
```

Bundle-enabled nodes always perform the startup catch-up even when the saved
Kafka offset is still within retention. A valid but old notification can refer
to source objects already deleted by the compactor.

For an empty database, initialization follows the same storage preference:
the configured genesis block is read from compacted bundle storage first, with
a fallback to the legacy per-block objects only when the bundle is absent.

### S3 Catch-up

When startup catch-up is required, KafkaUpdater synchronizes from S3:

```
┌─────────────────────────────────────────────────────────────┐
│                    S3 Catch-up Flow                          │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  Current DB block: N                                        │
│  Target block (from Kafka): M                               │
│                                                             │
│  1. Read compacted bundles in block order                   │
│     ├── Header: read the complete gzip JSON array           │
│     └── StateDiff: grouped Range reads (32 MiB by default)   │
│  2. On the first missing bundle, stop bundle probes         │
│  3. Read that height and all newer blocks from source S3    │
│  4. Apply every block to StateTree                          │
│                                                             │
│  Batch size controlled by: --init-task-queue-size           │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

Use `--bundle-range-size <MIB>` to tune the grouped Range request size. The
limit applies only to requests containing multiple entries; an oversized
single entry is still read by itself.
After the first missing bundle, retries resume from the in-memory latest block
and use only per-block source reads for the rest of the process.

### Configuration

Kafka + S3 config file (`--kafka-s3-config`):

```json
{
  "topic": "block-notifications",
  "brokers": "kafka1:9092,kafka2:9092",
  "partition": 0,
  "bucket_name": "state-diffs-bucket",
  "bundle_bucket_name": "compacted-state-diffs-bucket",
  "outer_bucket_name": "block-info-bucket",
  "offset_dir": "/path/to/offset",
  "s3_chain_id": "1",
  "version": "v1"
}
```

| Field | Description |
|-------|-------------|
| `topic` | Kafka topic for block change notifications |
| `brokers` | Kafka broker addresses |
| `partition` | Kafka partition to consume |
| `bucket_name` | S3 bucket for state diffs |
| `bundle_bucket_name` | Optional S3 bucket for compacted Header and StateDiff bundles; empty disables bundle reads |
| `outer_bucket_name` | S3 bucket for block info |
| `offset_dir` | Directory to persist Kafka offset |
| `s3_chain_id` | Chain identifier in S3 paths |
| `version` | Data version in S3 paths |

## HTTP Mode (Fallback)

### Architecture

```
┌─────────────────┐     ┌─────────────────┐
│   Modified Geth │     │   leafage-evm   │
│                 │     │                 │
│ trace_debank    │     │  HttpUpdater    │
│ Block RPC       │────►│                 │
│                 │     │  StateTree      │
└─────────────────┘     └─────────────────┘
```

### Update Flow

```
┌─────────────────────────────────────────────────────────────┐
│                    HTTP Polling Flow                         │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  Loop (every update_interval):                              │
│                                                             │
│  1. Get current block from StateTree                        │
│                                                             │
│  2. Query latest block number from Geth                     │
│     └── eth_blockNumber                                     │
│                                                             │
│  3. If new blocks available:                                │
│     a. Fetch block info and state diff via debank_block     │
│        └── trace_debankBlock(block_id)                      │
│     b. Handle reorg if parent not in StateTree              │
│        └── Walk back to find common ancestor                │
│     c. Apply to StateTree                                   │
│        └── tree.update_block(block_info, block_diff)        │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### Reorg Handling

HTTP mode handles chain reorganizations:

```
StateTree:  ... ── Block A ── Block B ── Block C (head)
New chain:  ... ── Block A ── Block B' ── Block C' ── Block D'

Detection:
  1. Fetch Block D' from Geth
  2. Check if parent (C') exists in StateTree
  3. If not, fetch C', check its parent (B')
  4. Continue until finding common ancestor (A)
  5. Apply B', C', D' in order
```

### Configuration

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--rpc-addr` | - | Geth RPC endpoint URL |
| `--update-interval` | 100ms | Polling interval |
| `--diff-depth-limit` | 64 | Max reorg depth to handle |

## Comparison

| Aspect | Kafka + S3 | HTTP |
|--------|------------|------|
| Latency | Lower (push-based) | Higher (polling) |
| Throughput | Higher (parallel S3 fetches) | Lower (sequential) |
| Reliability | Offset persistence, catch-up | Simple polling |
| Infrastructure | Kafka + S3 required | Only Geth RPC |
| Use Case | Production | Development/Fallback |

## Related Parameters

| Parameter | Description |
|-----------|-------------|
| `--kafka-s3-config` | Path to Kafka + S3 config JSON |
| `--rpc-addr` | Geth RPC address for HTTP mode |
| `--update-interval` | HTTP polling interval (ms) |
| `--diff-depth-limit` | Max block diffs in memory / reorg depth |
| `--init-task-queue-size` | Batch size for S3 catch-up (default: 256) |
| `--bundle-range-size <MIB>` | Target size for grouping multiple compacted StateDiff entries into one S3 Range request (default: 32 MiB); oversized single entries are read alone |

## Related Documentation

- [Architecture.md](Architecture.md) - Overall system architecture
- [StateManage.md](StateManage.md) - In-memory state tree and fork handling
- [Database.md](Database.md) - Database storage layout
- [DataSpec.md](DataSpec.md) - Data format specification for S3 and HTTP modes
- [Deploy](deploy/) - Deployment guide with Docker Compose
