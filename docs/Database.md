# Database Storage

This document describes the RocksDB storage layout for State Node and Archive Node modes.

## Overview

leafage-evm uses RocksDB with 6 Column Families:

| Column Family | Description |
|---------------|-------------|
| LatestBlockHash | Stores the latest block hash |
| BlockHashToBlockInfo | Maps block hash → block header |
| BlockNumToBlockHash | Maps block number → block hash |
| AddressToAccount | Maps address → account info |
| AddressToStorage | Maps (address, slot) → storage value |
| HashToCode | Maps code hash → bytecode |

## State Node Storage

State Node only stores the **latest state**. Simple key-value mapping with direct lookup.

### Key Layout

```
AddressToAccount:
  Key:   address (32 bytes)
  Value: RLP(SlimAccount)

AddressToStorage:
  Key:   address (32 bytes) || slot (32 bytes)
  Value: value (32 bytes, big-endian)

HashToCode:
  Key:   code_hash (32 bytes)
  Value: bytecode (variable length)
```

### Lookup

Direct `get()` operation - O(1) for latest state queries.

```
read_account(address):
    return db.get(AddressToAccount, address)

read_storage(address, slot):
    return db.get(AddressToStorage, address || slot)
```

## Archive Node Storage

Archive Node stores **all historical state** by appending block number to keys.

### Key Layout

```
AddressToAccount:
  Key:   address (32 bytes) || block_num (32 bytes, big-endian)
  Value: RLP(SlimAccount) or empty (for deleted accounts)

AddressToStorage:
  Key:   address (32 bytes) || slot (32 bytes) || block_num (32 bytes, big-endian)
  Value: value (32 bytes, big-endian)

HashToCode:
  Key:   code_hash (32 bytes)
  Value: bytecode (variable length)
```

### Dual-Write Strategy

When writing state at block N, archive mode writes **two entries**:

1. **Historical entry**: `address || block_num` → value
2. **Latest entry**: `address || u64::MAX` → value

This enables:
- O(1) lookup for latest state (block_num = u64::MAX)
- Efficient historical lookup using iterator

```rust
// Write account at block N
fn write_account(address, block_num, account):
    batch.put(address || block_num, account)      // Historical
    batch.put(address || u64::MAX, account)       // Latest (fast path)
```

### Historical State Lookup with Iterator

For historical queries, archive mode uses RocksDB's `seek_for_prev()`:

```
Query: get account state at block N

Keys in RocksDB (sorted):
  address || block_100 → account_v1
  address || block_200 → account_v2
  address || block_300 → account_v3
  address || u64::MAX  → account_v3 (latest)

seek_for_prev(address || block_250):
  → Returns (address || block_200, account_v2)
  → This is the correct state at block 250
```

#### Why This Works

1. RocksDB keys are sorted lexicographically
2. All versions of same address are grouped together (prefix = address)
3. Within a prefix, entries are sorted by block number
4. `seek_for_prev(key)` finds the largest key ≤ target key
5. Result is the state that was valid at the queried block

### Prefix Extractor Optimization

Archive mode configures RocksDB prefix extractors for efficient prefix-based operations:

```rust
// AddressToAccount: 32-byte prefix (address)
cf_opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(32));

// AddressToStorage: 64-byte prefix (address + slot)
cf_opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(64));
```

Benefits:
- Bloom filters work at prefix level
- Iterator seeks are optimized within prefix
- Reduces disk I/O for historical queries

## Iterator Tracking

Archive mode uses persistent RocksDB iterators for historical queries. Long-running iterators can block SST file deletion during compaction.

### Iterator Tracker

A background monitor tracks active iterators and enforces timeouts:

```
Environment Variable: ROCKSDB_ITERATOR_TIMEOUT_SECS (default: 0 = disabled)

Timeline:
  0s          → Iterator created, registered with tracker
  timeout     → Iterator marked as timed out, new operations return error
  2x timeout  → Iterator force-released, unregistered from tracker
```

### Lifecycle

```
┌─────────────────────────────────────────────────────────────┐
│                    StateDB Lifecycle                        │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  db_at(block_id)                                            │
│       │                                                     │
│       ▼                                                     │
│  ┌─────────────────┐                                        │
│  │ Create iterators│ ──► Register with IteratorTracker     │
│  │ (account, storage)                                       │
│  └────────┬────────┘                                        │
│           │                                                 │
│           ▼                                                 │
│  ┌─────────────────┐                                        │
│  │  Query state    │ ◄── Check timeout flag before each op │
│  │  (seek_for_prev)│                                        │
│  └────────┬────────┘                                        │
│           │                                                 │
│           ▼                                                 │
│  ┌─────────────────┐                                        │
│  │    Drop StateDB │ ──► Unregister from tracker           │
│  └─────────────────┘                                        │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### Metrics

| Metric | Description |
|--------|-------------|
| `active_iterators` | Current number of active StateDB instances |
| `timed_out_iterators` | Iterators marked as timed out |
| `force_released_iterators` | Cumulative count of force-released iterators |

## Storage Comparison

| Aspect | State Node | Archive Node |
|--------|------------|--------------|
| Storage Size (ETH) | ~90GB | ~360GB |
| Historical Queries | Latest only | Any block |
| Key Size (Account) | 32 bytes | 64 bytes |
| Key Size (Storage) | 64 bytes | 96 bytes |
| Lookup Method | Direct get() | seek_for_prev() or get() |
| Iterator Usage | No | Yes (for historical) |

## RocksDB Configuration

Both modes share optimized RocksDB settings:

```rust
// Write buffer and compaction
opts.set_write_buffer_size(256MB);
opts.set_max_bytes_for_level_base(256MB);
opts.set_max_compaction_bytes(2GB);

// Cache (shared HyperClockCache)
block_opts.set_block_cache(shared_cache);
block_opts.set_cache_index_and_filter_blocks(true);

// Bloom filters
block_opts.set_bloom_filter(10.0, prefix_mode);

// I/O optimization
opts.set_use_direct_io_for_flush_and_compaction(true);
```

### Environment Variables

| Variable | Description |
|----------|-------------|
| `ROCKSDB_MAX_OPEN_FILE` | Max open file descriptors |
| `ROCKSDB_DIRECT_IO` | Enable direct I/O for reads |
| `ROCKSDB_ITERATOR_TIMEOUT_SECS` | Iterator timeout (archive only) |

## Related Documentation

- [Architecture.md](Architecture.md) - Overall system architecture
- [StateManage.md](StateManage.md) - In-memory state tree and fork handling
- [StateUpdater.md](StateUpdater.md) - Kafka + S3 and HTTP update modes
- [DataSpec.md](DataSpec.md) - Data format specification
- [Deploy](deploy/) - Deployment guide with Docker Compose
