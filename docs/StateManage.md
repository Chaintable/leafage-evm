# State Management

This document describes how leafage-evm manages blockchain state in memory and on disk.

## Overview

leafage-evm uses a hybrid state management approach:

- **In-memory**: Recent blocks that may be subject to reorganization (fork)
- **On-disk**: Finalized blocks that are considered immutable

This design ensures:
1. Fast state queries for recent blocks
2. Support for chain reorganizations without database rollback
3. Only finalized (stable) state is persisted to disk

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      StateTree                               │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  ┌─────────────┐                                            │
│  │   latest    │ ──► Points to head of main chain           │
│  └─────────────┘                                            │
│                                                             │
│  ┌─────────────┐     ┌─────────────┐                        │
│  │hash_diff_map│     │num_diff_map │                        │
│  │ hash → layer│     │ num → layer │                        │
│  └─────────────┘     └─────────────┘                        │
│         │                   │                               │
│         └─────────┬─────────┘                               │
│                   ▼                                         │
│         ┌─────────────────┐                                 │
│         │  LinkedDiffLayer │  ◄── Linked list of blocks     │
│         │     (memory)     │                                │
│         └────────┬────────┘                                 │
│                  │                                          │
│                  ▼                                          │
│         ┌─────────────────┐                                 │
│         │  CacheDiskLayer │  ◄── Cache + DB interface       │
│         └────────┬────────┘                                 │
│                  │                                          │
│                  ▼                                          │
│         ┌─────────────────┐                                 │
│         │ RocksDB / MDBX  │  ◄── Finalized state only       │
│         └─────────────────┘                                 │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

## Core Components

### StateTree

The main state management structure that coordinates in-memory and on-disk state.

```rust
pub struct StateTree<DB> {
    latest: RwLock<Arc<LinkedDiffLayer>>,           // Head of main chain
    hash_diff_map: RwLock<HashMap<H256, Arc<LinkedDiffLayer>>>,  // Block hash index
    num_diff_map: RwLock<HashMap<u64, Arc<LinkedDiffLayer>>>,    // Block number index
    config: StateTreeConfig,
    db: DB,
}
```

| Field | Description |
|-------|-------------|
| `latest` | Points to the latest block on the main chain |
| `hash_diff_map` | Maps block hash to its diff layer (supports fork blocks) |
| `num_diff_map` | Maps block number to diff layer on main chain |
| `config` | Configuration (depth limit, cache sizes) |
| `db` | Underlying database (RocksDB/MDBX) |

### LinkedDiffLayer

A linked list structure representing block state layers:

```rust
pub enum LinkedDiffLayer {
    DiffLayer(DiffLayer),         // Block state diff (in memory)
    CacheDiskLayer(CacheDiskLayer), // Cache layer + disk interface
    Empty,                        // Empty placeholder
}
```

```
Block N (latest)     Block N-1          Block N-2              Disk
┌───────────────┐   ┌───────────────┐   ┌───────────────┐   ┌─────────────┐
│   DiffLayer   │──►│   DiffLayer   │──►│   DiffLayer   │──►│ CacheDisk   │──► RocksDB
│  (block diff) │   │  (block diff) │   │  (block diff) │   │   Layer     │
└───────────────┘   └───────────────┘   └───────────────┘   └─────────────┘
```

### DiffLayer

Stores state changes for a single block:

```rust
pub struct DiffLayer {
    pub block_info: Arc<Block<H256>>,          // Block header
    pub block_diff: Arc<BlockStorageDiff>,     // Original diff data
    pub accounts: HashMap<H256, Option<AccountInfo>>,  // Account changes
    pub storage: HashMap<(H256, H256), U256>,  // Storage changes
    pub contracts: HashMap<H256, Bytecode>,    // New contract codes
    pub next: RwLock<Arc<LinkedDiffLayer>>,    // Parent layer
}
```

### CacheDiskLayer

The bottom layer that interfaces with the database:

```rust
pub struct CacheDiskLayer {
    accounts: Cache<H256, Option<AccountInfo>>,   // Account cache
    storages: Cache<(H256, H256), U256>,          // Storage cache
    contracts: Cache<H256, Bytecode>,             // Code cache
    block_hashes: Cache<u64, H256>,               // Block hash cache
    old_diff_layer: Mutex<Option<Arc<LinkedDiffLayer>>>, // Last committed
}
```

### HybridStateDB

Combines memory layers with disk storage for state queries:

```rust
pub struct HybridStateDB<DB> {
    pub memory_layer: Arc<LinkedDiffLayer>,  // In-memory state
    pub statedb: DB,                         // Disk state
}
```

## State Query Flow

When querying state at a specific block:

```
┌─────────────────────────────────────────────────────────────┐
│                    State Query Flow                          │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  Query: get storage at block N, address A, slot S           │
│                                                             │
│  1. Find DiffLayer for block N (via hash_diff_map)          │
│                                                             │
│  2. Search linked list from top to bottom:                  │
│     ┌─────────────────────────────────────────────────┐     │
│     │  DiffLayer (Block N)                            │     │
│     │  └─► Check storage HashMap for (A, S)           │     │
│     │      ├─► Found: Return value                    │     │
│     │      └─► Not found: Continue to next layer      │     │
│     └─────────────────────────────────────────────────┘     │
│                         │                                   │
│                         ▼                                   │
│     ┌─────────────────────────────────────────────────┐     │
│     │  DiffLayer (Block N-1)                          │     │
│     │  └─► Check storage HashMap for (A, S)           │     │
│     │      ├─► Found: Return value                    │     │
│     │      └─► Not found: Continue to next layer      │     │
│     └─────────────────────────────────────────────────┘     │
│                         │                                   │
│                         ▼                                   │
│                        ...                                  │
│                         │                                   │
│                         ▼                                   │
│     ┌─────────────────────────────────────────────────┐     │
│     │  CacheDiskLayer                                 │     │
│     │  └─► Check cache for (A, S)                     │     │
│     │      ├─► Cache hit: Return value                │     │
│     │      └─► Cache miss: Query RocksDB              │     │
│     │          └─► Insert into cache, return value    │     │
│     └─────────────────────────────────────────────────┘     │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

## Block Update and Finalization

### Adding New Block

```
┌─────────────────────────────────────────────────────────────┐
│                    update_block() Flow                       │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  1. Check if block already exists (by hash)                 │
│     └─► If exists, skip                                     │
│                                                             │
│  2. Find parent layer in hash_diff_map                      │
│     └─► If not found, return error                          │
│                                                             │
│  3. Create new DiffLayer                                    │
│     └─► Link to parent layer                                │
│                                                             │
│  4. Insert into hash_diff_map and num_diff_map              │
│                                                             │
│  5. If new block extends main chain:                        │
│     └─► Update `latest` pointer                             │
│     └─► Call cap_diff_to_db() to finalize old blocks        │
│                                                             │
│  6. If new block is a fork (lower height than latest):      │
│     └─► Only add to hash_diff_map (don't update latest)     │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

### Finalization (cap_diff_to_db)

Only blocks beyond the `diff_tree_depth_limit` are persisted to disk:

```
┌─────────────────────────────────────────────────────────────┐
│                   cap_diff_to_db() Flow                      │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  diff_tree_depth_limit = 64 (default)                       │
│                                                             │
│  Before (65 blocks in memory):                              │
│                                                             │
│  Block N ──► N-1 ──► ... ──► N-63 ──► N-64 ──► CacheDisk    │
│  (latest)              (depth=64)   (depth=65)              │
│                                                             │
│  Finalization:                                              │
│  1. Walk linked list to find all DiffLayers                 │
│  2. If depth > limit (65 > 64):                             │
│     └─► Commit Block N-64 to database                       │
│     └─► Update CacheDiskLayer.old_diff_layer                │
│     └─► Reconnect N-63 to CacheDiskLayer                    │
│     └─► Clear invalidated cache entries                     │
│                                                             │
│  After (64 blocks in memory):                               │
│                                                             │
│  Block N ──► N-1 ──► ... ──► N-63 ──► CacheDiskLayer        │
│  (latest)              (depth=64)     (N-64 in DB)          │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

## Fork Handling

All potentially forkable blocks are kept in memory:

```
Main chain:    Block A ──► Block B ──► Block C ──► Block D (latest)
                              │
Fork:                         └──► Block B' ──► Block C'

Memory state:
  hash_diff_map = {
    A: DiffLayer(A),
    B: DiffLayer(B),
    C: DiffLayer(C),
    D: DiffLayer(D),
    B': DiffLayer(B'),  // Fork block
    C': DiffLayer(C'),  // Fork block
  }

  num_diff_map = {
    100: DiffLayer(A),
    101: DiffLayer(B),   // Main chain only
    102: DiffLayer(C),
    103: DiffLayer(D),
  }

  latest = DiffLayer(D)
```

Key points:
- Fork blocks are stored in `hash_diff_map` but NOT in `num_diff_map`
- `num_diff_map` only contains main chain blocks
- Fork blocks can still be queried by hash
- When fork becomes main chain, `latest` and `num_diff_map` are updated

## Configuration

```rust
pub struct StateTreeConfig {
    pub diff_tree_depth_limit: usize,  // Max blocks in memory (default: 64)
    pub account_cache_size: usize,     // Account cache size (default: 1000000)
    pub storage_cache_size: usize,     // Storage cache size (default: 5000000)
    pub code_cache_size: usize,        // Code cache size (default: 100000)
}
```

| Parameter | CLI Flag | Default | Description |
|-----------|----------|---------|-------------|
| `diff_tree_depth_limit` | `--diff-depth-limit` | 64 | Max unfinialized blocks in memory |
| `account_cache_size` | `--account-cache-size` | 200000 | Account cache entries |
| `storage_cache_size` | `--storage-cache-size` | 2000000 | Storage cache entries |
| `code_cache_size` | `--code-cache-size` | 200000 | Code cache entries |

## Design Rationale

### Why keep unfinalized blocks in memory?

1. **Chain Reorganization**: Ethereum and other chains can have temporary forks. Keeping recent blocks in memory allows seamless handling of reorgs without database rollback.

2. **Finality**: Most chains consider blocks finalized after a certain depth (e.g., 64 blocks for Ethereum). Only finalized blocks need persistence.

3. **Performance**: In-memory HashMap lookups are much faster than database queries for recent state.

### Why use a linked list structure?

1. **Incremental State**: Each block only stores its state diff, not the full state. This reduces memory usage.

2. **Fork Support**: Multiple chains can share common ancestors through the linked structure.

3. **Efficient Queries**: State queries walk the chain from top to bottom, finding the most recent value for any key.

## Related Documentation

- [Architecture.md](Architecture.md) - Overall system architecture
- [Database.md](Database.md) - Database storage layout
- [StateUpdater.md](StateUpdater.md) - How state updates are received
- [DataSpec.md](DataSpec.md) - Data format specification
- [Deploy](deploy/) - Deployment guide with Docker Compose
