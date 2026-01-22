# Architecture

## Overview

leafage-evm is a lightweight EVM executor built with [alloy](https://github.com/alloy-rs/alloy) and [revm](https://github.com/bluealloy/revm). It focuses on **state queries** (`eth_call`, `eth_estimateGas`, etc.) and does **not store transaction data**. State updates are received via Kafka + S3, rather than P2P synchronization.

```
┌─────────────────────────────────────────────────────────────┐
│                     leafage-evm                             │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────┐     │
│  │  EthApi     │    │  DebankApi  │    │   PreApi    │     │
│  └──────┬──────┘    └──────┬──────┘    └──────┬──────┘     │
│         │                  │                  │             │
│         └──────────────────┼──────────────────┘             │
│                            │                                │
│                    ┌───────▼───────┐                        │
│                    │  EvmExecutor  │                        │
│                    │    (revm)     │                        │
│                    └───────┬───────┘                        │
│                            │                                │
│                    ┌───────▼───────┐                        │
│                    │   StateTree   │  ← Recent block diffs  │
│                    └───────┬───────┘    (in memory)         │
│                            │                                │
│                    ┌───────▼───────┐                        │
│                    │    RocksDB    │  ← Persisted state     │
│                    │    / MDBX     │                        │
│                    └───────────────┘                        │
│                                                             │
└─────────────────────────────────────────────────────────────┘
                            ▲
                            │ State Updates
              ┌─────────────┴─────────────┐
              │                           │
      ┌───────▼───────┐           ┌───────▼───────┐
      │     Kafka     │           │      S3       │
      │ (block notify)│           │ (block data)  │
      └───────────────┘           └───────────────┘
```

## Crate Structure

```
leafage-evm/
├── bin/leafage-evm/           # CLI binary
│   ├── main.rs                # Entry point, Jemalloc setup
│   ├── standalone.rs          # Server startup and CLI args
│   ├── updater/               # State update handlers
│   │   ├── kafka_updater.rs   # Kafka + S3 mode (primary)
│   │   └── http_updater.rs    # HTTP polling mode (fallback)
│   ├── initializer/           # Component initialization
│   └── warm/                  # Warmup logic
│
├── crates/
│   ├── leafage-evm-types/     # Shared type definitions
│   │   ├── primitives.rs      # Address, H256, U256, AccountInfo
│   │   ├── storage.rs         # BlockStorageDiff
│   │   └── rpc.rs             # JSON-RPC types
│   │
│   ├── leafage-evm-storage/   # Database and state management
│   │   ├── interface.rs       # StateDB trait
│   │   ├── state_tree/        # In-memory state diff layers
│   │   │   ├── tree.rs        # StateTree (linked-list structure)
│   │   │   └── layer.rs       # Individual block diff layer
│   │   └── db_impl/           # Database backends
│   │       ├── rocksdb_impl/  # RocksDB implementation
│   │       └── mdbx_impl/     # MDBX implementation
│   │
│   ├── leafage-evm-rpc/       # JSON-RPC implementation
│   │   ├── api/               # RPC trait definitions
│   │   │   ├── eth.rs         # eth_* methods
│   │   │   ├── debank.rs      # DeBankApi methods
│   │   │   └── pre.rs         # pre_* methods
│   │   └── api_impl/          # RPC implementations
│   │       ├── mainnet/       # Ethereum mainnet executor
│   │       ├── op/            # Optimism executor
│   │       ├── bsc/           # BSC executor
│   │       └── cosmos/        # Cosmos EVM executor
│   │
│   └── leafage-evm-chains/    # Chain-specific logic
│       ├── bsc/               # BSC precompiles (tendermint, IAVL)
│       └── cosmos/            # Cosmos precompiles (p256, bech32)
```

## State Management

See [StateManage.md](StateManage.md) for detailed documentation on state management internals.

### StateTree (Linked-List Structure)

State is managed as a linked-list of block diffs in memory:

```
┌─────────────────────┐
│  Latest Block (Head)│ ─► Block N state diff
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│     Block N-1       │ ─► Block N-1 state diff
└──────────┬──────────┘
           │
           ▼
          ...
           │
           ▼
┌─────────────────────┐
│     Block N-63      │ ─► Block N-63 state diff
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│  RocksDB (Base)     │ ─► Persisted state
└─────────────────────┘
```

### State Query Flow

1. Query arrives for block N, address A, storage slot S
2. Search StateTree layers from top (latest) to bottom
3. If found in memory layer, return immediately
4. If not found, query RocksDB base state
5. Return result

### Block Update Flow

1. Receive block notification from Kafka
2. Fetch block info and state diff from S3
3. Push new diff layer to StateTree head
4. If StateTree exceeds depth limit (default 64):
   - Persist oldest layer to RocksDB
   - Remove from StateTree

### Node Modes

leafage-evm supports two node modes. See [Database.md](Database.md) for detailed storage layout.

#### State Node (Default)

- Only supports **latest state** queries
- Recent block diffs in StateTree (default 64 blocks)
- RocksDB stores only current state snapshot
- Storage: ~90GB for ETH mainnet (as of 2025.1)

```
Query Flow:
  eth_call(block=latest) → StateTree → RocksDB (latest only)
  eth_call(block=12345)  → Error: historical state not available
```

#### Archive Node (`--archive`)

- Supports **any historical block** queries
- All state versions stored in RocksDB with block number suffix
- Uses RocksDB iterator with `seek_for_prev()` for efficient historical lookup
- Storage: ~360GB for ETH mainnet (as of 2025.1)

```
Query Flow:
  eth_call(block=latest) → StateTree → RocksDB (u64::MAX key, O(1))
  eth_call(block=12345)  → RocksDB iterator seek_for_prev(addr || 12345)
```

#### Comparison

| Aspect | State Node | Archive Node |
|--------|------------|--------------|
| Historical Queries | Latest only | Any block |
| Storage (ETH) | ~90GB | ~360GB |
| RocksDB Key (Account) | `address` | `address \|\| block_num` |
| RocksDB Key (Storage) | `address \|\| slot` | `address \|\| slot \|\| block_num` |
| Lookup Method | Direct get() | get() or seek_for_prev() |
| Use Case | DeFi apps, wallets | Block explorers, analytics |

## State Update

See [StateUpdater.md](StateUpdater.md) for detailed documentation on state update mechanisms.

### Kafka + S3 Mode (Primary)

```
Kafka Consumer                           S3 Storage
     │                                       │
     │ Block change notification             │
     ▼                                       │
┌─────────────┐                              │
│ KafkaUpdater│──── Fetch block info ───────►│
│             │◄─── Block header + hash ─────│
│             │                              │
│             │──── Fetch state diff ───────►│
│             │◄─── BlockStorageDiff ────────│
└──────┬──────┘
       │
       ▼
   StateTree.push(block_info, state_diff)
```

### HTTP Mode (Fallback)

Polls `trace_debankBlock` RPC from a modified Geth instance. Used when Kafka is unavailable.

## EVM Execution

Each chain type has its own executor:

| EVM Type | Executor | Special Features |
|----------|----------|------------------|
| mainnet | MainnetExecutor | Standard Ethereum EVM |
| op | OpExecutor | L2 gas calculation, OVM precompiles |
| bsc | BscExecutor | Parlia validators, tendermint/IAVL precompiles |
| cosmos | CosmosExecutor | bech32 address, p256 signatures |

### Execution Flow

```
RPC Request (eth_call)
       │
       ▼
┌─────────────────┐
│  Build EVM Env  │  ← Block context, tx params
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ StateDB Wrapper │  ← Implements revm::DatabaseRef
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│   revm Execute  │  ← EVM bytecode execution
└────────┬────────┘
         │
         ▼
    Return Result
```

## Key Traits

### StateDBRead (leafage-evm-storage/src/db.rs)

```rust
pub trait StateDBRead {
    fn read_account(&self, address: H256) -> Result<Option<NewAccount>>;
    fn read_storage(&self, address: H256, key: H256) -> Result<U256>;
    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>>;
    fn read_block_hash(&self, block_num: u64) -> Result<H256>;
    fn read_block_info(&self, block_hash: H256) -> Result<Option<Block<H256>>>;
    fn read_latest_block_hash(&self) -> Result<H256>;
}
```

### EvmExecutor (leafage-evm-rpc/src/api_impl/core.rs)

```rust
pub(crate) trait EvmExecutor: Sync + Send + 'static {
    type Tx: TxSetter + TransactionTrait + Clone;
    type TransactionError: ToJsonRpcError + GetTransactionError;
    type EvmHaltReason: std::fmt::Debug + Clone;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self, block_env: &BlockEnv, request: CallRequest, db: StateDB, chain_id: u64,
    ) -> RpcResult<Self::Tx>;

    fn transact<StateDB: DatabaseRef>(
        &self, block_env: &BlockEnv, state: StateDB, tx: Self::Tx,
    ) -> Result<ExecutionResult<Self::EvmHaltReason>, EVMError<...>>;

    fn inspect_tx_commit<StateDB, R, F>(
        &self, block_env: &BlockEnv, state: StateDB, inspector_cfg: TracingInspectorConfig,
        inspector_collect: F, tx: Self::Tx,
    ) -> Result<(ExecutionResult<Self::EvmHaltReason>, R), EVMError<...>>;
}
```

### ApiBase (leafage-evm-rpc/src/api_impl/core.rs)

```rust
pub(crate) trait ApiBase: Sync + Send + 'static {
    type DB;
    type SpecId;

    fn db(&self) -> &Self::DB;
    fn evm_cfg(&self) -> &EvmCfg<Self::SpecId>;
    fn historical_client(&self) -> Option<&HttpClient>;
    fn historical_height(&self) -> Option<u64>;
}
```

**Historical RPC Forwarding**: The `historical_client` and `historical_height` fields enable forwarding queries to an external RPC endpoint for blocks where block diff data is unavailable. This is primarily used for:
- **Optimism pre-bedrock**: Blocks before the Bedrock upgrade don't have block diff data in S3
- **Other forked chains**: Any chain where historical state diffs are not available

When a query targets a block number below `historical_height`, the request is automatically forwarded to `historical_client` instead of being processed locally.

## Data Flow Summary

```
External Data Sources          leafage-evm                    Clients
       │                           │                             │
       │   ┌───────────────────────┼───────────────────────┐     │
       │   │                       │                       │     │
    Kafka ─┼─► KafkaUpdater ──────►│                       │     │
       │   │        │              │                       │     │
      S3 ──┼────────┘              │                       │     │
       │   │                       ▼                       │     │
       │   │              ┌─────────────────┐              │     │
       │   │              │   StateTree     │              │     │
       │   │              │  (in-memory)    │              │     │
       │   │              └────────┬────────┘              │     │
       │   │                       │                       │     │
       │   │              ┌────────▼────────┐              │     │
       │   │              │ RocksDB / MDBX  │              │     │
       │   │              │  (persisted)    │              │     │
       │   │              └────────┬────────┘              │     │
       │   │                       │                       │     │
       │   │              ┌────────▼────────┐              │◄────┤
       │   │              │  EvmExecutor    │──────────────┼────►│
       │   │              │    (revm)       │   JSON-RPC   │     │
       │   │              └─────────────────┘              │     │
       │   │                                               │     │
       │   └───────────────────────────────────────────────┘     │
       │                                                         │
```
