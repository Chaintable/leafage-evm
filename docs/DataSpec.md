# Data Specification

This document describes the data formats required by leafage-evm for state updates.

## Overview

leafage-evm supports two modes for receiving state updates:

| Mode | Data Source | Format |
|------|-------------|--------|
| Kafka + S3 | S3 objects + Kafka notifications | RLP-encoded state diff + JSON block info |
| HTTP | `trace_debankBlock` RPC | JSON response with RLP-encoded state diff |

## Core Data Structures

### BlockStorageDiff

The core state diff structure, RLP-encoded.

```rust
struct BlockStorageDiff {
    hash: H256,              // Current block's state root
    parent_hash: H256,       // Parent block's state root
    new_accounts: Vec<NewAccount>,
    deleted_accounts: Vec<H256>,  // Hashed addresses (keccak256)
    storage_diffs: Vec<AccountStorageDiff>,
    new_codes: Vec<NewCode>,
}

struct NewAccount {
    address: H256,           // keccak256(address)
    balance: U256,
    nonce: u64,
    code_hash: H256,
}

struct AccountStorageDiff {
    address: H256,           // keccak256(address)
    diffs: Vec<IndexValuePair>,
}

struct IndexValuePair {
    index: H256,             // keccak256(storage_slot)
    value: U256,
}

struct NewCode {
    code_hash: H256,
    code: Bytes,
}
```

**Important**: All addresses and storage keys are hashed using `keccak256` before storage.

### DebankOutPut

The response format for `trace_debankBlock` RPC.

```rust
struct DebankOutPut {
    header: Header,          // Standard Ethereum block header
    state_diff: Bytes,       // RLP-encoded BlockStorageDiff
}
```

---

## HTTP Mode: `trace_debankBlock` RPC

### Method

```
trace_debankBlock
```

### Parameters

| Name | Type | Description |
|------|------|-------------|
| `block_id` | `BlockId` | Block identifier (number, hash, or tag) |

`BlockId` can be:
- Block number: `{ "blockNumber": "0x123" }` or `"0x123"`
- Block hash: `{ "blockHash": "0x..." }`
- Tag: `"latest"`, `"pending"`, `"earliest"`

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "header": {
      "hash": "0x...",
      "parentHash": "0x...",
      "number": "0x123",
      "stateRoot": "0x...",
      "timestamp": "0x...",
      "gasLimit": "0x...",
      "gasUsed": "0x...",
      "baseFeePerGas": "0x...",
      "miner": "0x...",
      ...
    },
    "state_diff": "0x..."  // RLP-encoded BlockStorageDiff as hex string
  }
}
```

### Example Request

```bash
curl -X POST http://localhost:8545 \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "method": "trace_debankBlock",
    "params": ["0x123"],
    "id": 1
  }'
```

### Implementation Notes

1. The `state_diff` field contains the RLP-encoded `BlockStorageDiff` representing state changes from the previous block to the current block
2. For genesis block (block 0), `state_diff` should contain the initial state
3. If no state changes occurred (e.g., empty block), return a `BlockStorageDiff` with:
   - `hash` = current block's state root
   - `parent_hash` = parent block's state root
   - Empty `new_accounts`, `deleted_accounts`, `storage_diffs`, `new_codes`

---

## S3 Mode: Data Upload Specification

### S3 Path Structure

```
s3://{bucket_name}/{chain_id}/{version}/{data_type}
```

| Component | Description | Example |
|-----------|-------------|---------|
| `bucket_name` | S3 bucket name | `state-diffs-bucket` |
| `chain_id` | Chain identifier | `1` (Ethereum), `56` (BSC) |
| `version` | Data version (optional) | `v1`, `v2` |

### Required S3 Objects

#### 1. Block Info

**Path**: `s3://{bucket_name}/{chain_id}/{version}/{block_hash}/block`

**Format**: Gzip-compressed JSON

**Content**: Standard Ethereum block structure

```json
{
  "header": {
    "hash": "0x...",
    "parentHash": "0x...",
    "number": "0x123",
    "stateRoot": "0x...",
    "timestamp": "0x...",
    ...
  },
  "transactions": ["0x...", "0x..."],
  "uncles": []
}
```

#### 2. State Diff

**Path**: `s3://{bucket_name}/{chain_id}/{version}/{state_root}/stateDiff`

**Format**: Raw RLP-encoded `BlockStorageDiff` (not compressed)

**Content**: Binary RLP data

#### 3. Block Hash Index (Optional, for number-based lookup)

**Path**: `s3://{outer_bucket_name}/{chain_id}/{version}/{block_number}/{block_hash}`

**Format**: Gzip-compressed JSON

**Content**:
```json
{
  "validation_hash": 12345,
  "is_fork": false
}
```

When multiple blocks exist at the same height (fork), only the canonical block should have `is_fork: false`.

### Kafka Notification Format

**Topic**: Configured via `kafka_s3_config.topic`

**Message Format**: Gzip-compressed JSON

```json
{
  "changeType": 1,
  "newBlocks": [
    {
      "hash": "0x...",
      "parentHash": "0x...",
      "blockNumber": 12345
    }
  ],
  "dropBlocks": []
}
```

| Field | Type | Description |
|-------|------|-------------|
| `changeType` | `u64` | Type of change (1 = new block) |
| `newBlocks` | `Array` | List of new blocks to process |
| `dropBlocks` | `Array` | List of blocks to drop (reorg) |

### Upload Workflow

For each new block:

1. **Upload Block Info**
   ```
   PUT s3://{bucket}/{chain_id}/{version}/{block_hash}/block
   Content: gzip(json(block_info))
   ```

2. **Upload State Diff** (if state changed)
   ```
   PUT s3://{bucket}/{chain_id}/{version}/{state_root}/stateDiff
   Content: rlp_encode(BlockStorageDiff)
   ```

3. **Send Kafka Notification**
   ```
   Topic: {configured_topic}
   Message: gzip(json(KafkaBlockChangeNotification))
   ```

### S3 Configuration

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

---

## RLP Encoding Reference

### BlockStorageDiff Encoding Order

```
[
  hash,
  parent_hash,
  [new_accounts...],
  [deleted_accounts...],
  [storage_diffs...],
  [new_codes...]
]
```

### NewAccount Encoding Order

```
[address, balance, nonce, code_hash]
```

### AccountStorageDiff Encoding Order

```
[address, [[index, value], [index, value], ...]]
```

### NewCode Encoding Order

```
[code_hash, code]
```

---

## Block Query APIs (Header Only)

leafage-evm does not store transaction data. The following block query APIs return **header only** (no transactions):

| Method | Description |
|--------|-------------|
| `eth_getBlockByNumber` | Returns block header by number |
| `eth_getBlockByHash` | Returns block header by hash |
| `debank_getLatestBlock` | Returns latest block header |
| `debank_getBlockByHeight` | Returns block header by height |
| `debank_getBlockById` | Returns block header by hash or number |

### Response Format

These APIs return a `Block` structure with:
- `header`: Full block header information
- `transactions`: Always empty (`[]`)
- `uncles`: Always empty (`[]`)

```json
{
  "header": {
    "hash": "0x...",
    "parentHash": "0x...",
    "number": "0x123",
    "stateRoot": "0x...",
    "timestamp": "0x...",
    "gasLimit": "0x...",
    "gasUsed": "0x...",
    "baseFeePerGas": "0x...",
    "miner": "0x...",
    ...
  },
  "transactions": [],
  "uncles": []
}
```

### Rationale

leafage-evm is designed as a lightweight EVM executor focused on state queries (`eth_call`, `eth_estimateGas`). It does not:
- Store full transaction data
- Process transaction receipts
- Maintain transaction indices

For transaction-related queries, use a full node or block explorer API.

---

## Validation

### Required Fields

For `trace_debankBlock`:
- `header.hash` must match the requested block
- `header.parentHash` must be valid
- `header.stateRoot` must match `state_diff.hash`
- `state_diff.parent_hash` must match parent block's state root

### Consistency Checks

1. All addresses in `new_accounts` and `storage_diffs` must be keccak256 hashed
2. All storage indices must be keccak256 hashed
3. `code_hash` in `new_accounts` must match the hash of corresponding code in `new_codes`

## Related Documentation

- [Architecture.md](Architecture.md) - Overall system architecture
- [StateManage.md](StateManage.md) - In-memory state tree and fork handling
- [StateUpdater.md](StateUpdater.md) - Kafka + S3 and HTTP update modes
- [Database.md](Database.md) - Database storage layout
- [Deploy](deploy/) - Deployment guide with Docker Compose
