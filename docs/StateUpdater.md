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
│     └── s3://{bucket}/{chain_id}/{version}/block/{hash}     │
│                                                             │
│  3. Fetch state diff from S3 (parallel)                     │
│     └── s3://{bucket}/{chain_id}/{version}/diff/{state_root}│
│     └── Skip if state_root unchanged (empty diff)           │
│                                                             │
│  4. Apply updates to StateTree                              │
│     └── tree.update_block(block_info, block_diff)           │
│                                                             │
│  5. Commit offset after persistence                         │
│     └── write_offset(offset_dir, offset + 1)                │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### Offset Management

KafkaUpdater maintains Kafka consumer offset for crash recovery:

```
Startup:
  1. Read persisted offset from offset_dir
  2. Fetch Kafka watermarks (lowest, latest)
  3. Decision:
     ├── offset >= lowest ──► Resume from offset
     └── offset < lowest or missing ──► Sync from S3, start from latest
```

### S3 Catch-up

When offset is invalid (missing or expired), KafkaUpdater synchronizes from S3:

```
┌─────────────────────────────────────────────────────────────┐
│                    S3 Catch-up Flow                          │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  Current DB block: N                                        │
│  Target block (from Kafka): M                               │
│                                                             │
│  for block_num in (N+1)..=M:                                │
│      1. Fetch block info by number from S3                  │
│      2. Fetch state diff from S3                            │
│      3. Apply to StateTree                                  │
│                                                             │
│  Batch size controlled by: --init-task-queue-size           │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### Configuration

Kafka + S3 config file (`--kafka-s3-config`):

```json
{
  "topic": "block-notifications",
  "brokers": "kafka1:9092,kafka2:9092",
  "partition": 0,
  "bucket_name": "state-diffs-bucket",
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

## Related Documentation

- [DataSpec.md](DataSpec.md) - Data format specification for S3 and HTTP modes
