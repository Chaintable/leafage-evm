# leafage-evm

[中文文档](README_cn.md)

leafage-evm is a lightweight EVM executor built with [alloy](https://github.com/alloy-rs/alloy) and [revm](https://github.com/bluealloy/revm). It focuses on **state queries** (`eth_call`, `eth_estimateGas`, etc.) and does **not store transaction data**. State updates are received via Kafka + S3, rather than P2P synchronization.

## Features

- **Two Node Modes**:
  - **State Node**: Only retains recent block states (default 64 blocks), ~90GB for ETH mainnet (as of 2025.1)
  - **Archive Node**: Retains complete historical state, ~360GB for ETH mainnet (as of 2025.1)
- **Multi-chain Support**: Ethereum mainnet, Optimism, BSC, Cosmos EVM
- **Multiple Database Backends**: RocksDB (default), MDBX
- **Data Migration**: Import initial state from Geth snapshots

## Supported Write Node Repositories

Any EVM-compatible chain can potentially be supported. The following chains are currently supported:

| Chain | Repository |
|-------|------------|
| ETH | [Chaintable/go-ethereum](https://github.com/Chaintable/go-ethereum) |
| AVAX | [Chaintable/coreth](https://github.com/Chaintable/coreth) |
| OP Stack (OP, Base, etc.) | [Chaintable/op-geth](https://github.com/Chaintable/op-geth) |
| Gnosis | [Chaintable/erigon](https://github.com/Chaintable/erigon) |
| Tempo | [Chaintable/reth-x](https://github.com/Chaintable/reth-x) |
| Bitlayer | [Chaintable/bitlayer-l2](https://github.com/Chaintable/bitlayer-l2) |
| Oasys | [Chaintable/oasys-validator](https://github.com/Chaintable/oasys-validator) |
| Kava | [Chaintable/kava](https://github.com/Chaintable/kava) |
| IoTeX | [Chaintable/iotex-core-x](https://github.com/Chaintable/iotex-core-x) |

## Supported JSON-RPC Methods

### eth_*

| Method | Description |
|--------|-------------|
| `eth_call` | Execute a contract call |
| `eth_multiCall` | Batch contract calls |
| `eth_blockNumber` | Get current block number |
| `eth_getBalance` | Get account balance |
| `eth_getBlockByNumber` | Get block by number |
| `eth_getBlockByHash` | Get block by hash |
| `eth_getCode` | Get contract code |
| `eth_getStorageAt` | Get storage slot data |
| `eth_getTransactionCount` | Get account nonce |
| `eth_chainId` | Get chain ID |
| `eth_baseFee` | Get base fee |

### DeBankApi (no namespace prefix)

| Method | Description |
|--------|-------------|
| `version` | Get version info |
| `getAddressNonce` | Get account nonce |
| `getAddressBalance` | Get account balance |
| `getAddressCode` | Get contract code |
| `getStorageAt` | Get storage slot data |
| `contractMultiCall` | Batch contract calls |
| `simulateTransactions` | Simulate transaction execution |
| `estimateGas` | Estimate gas |
| `getLatestBlock` | Get latest block |
| `getBlockByHeight` | Get block by height |
| `getBlockById` | Get block by hash |
| `blockIsValid` | Validate block |

> **Note**: Block query methods (`eth_getBlockByNumber`, `eth_getBlockByHash`, `getLatestBlock`, `getBlockByHeight`, `getBlockById`) return **header only** - `transactions` and `uncles` are always empty. leafage-evm does not store transaction data.

### pre_*

| Method | Description |
|--------|-------------|
| `pre_traceCall` | Pre-execute call trace |
| `pre_traceMany` | Batch pre-execute traces |

## Build

**Requirements**: Rust 1.79+

```bash
cargo build --release
```

Docker build:

```bash
docker build -t leafage-evm .
```

## Usage

### Start Server

```bash
RUST_LOG=info ./target/release/leafage-evm standalone \
  --db-path /path/to/db \
  --listen-addr 0.0.0.0:8545 \
  --rpc-addr http://geth:8545 \
  --evm-type mainnet \
  --chain-cfg 1
```

### Main Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--db-path` | - | Database path (required) |
| `--listen-addr` | 0.0.0.0:8545 | RPC listen address |
| `--rpc-addr` | - | Geth RPC address (for HTTP mode state updates) |
| `--evm-type` | mainnet | EVM type: mainnet/op/bsc/cosmos |
| `--chain-cfg` | 1 | Chain ID |
| `--db-type` | rocksdb | Database type: rocksdb/mdbx |
| `--db-cache` | 2048 | Database cache size (MB) |
| `--diff-depth-limit` | 64 | Block diff depth retained in memory |
| `--archive` | false | Enable archive mode |
| `--prometheus-addr` | - | Prometheus metrics address |
| `--kafka-s3-config` | - | Path to Kafka + S3 config JSON file |
| `--max-connections` | 5000 | Maximum concurrent RPC connections |
| `--rpc-timeout` | 10000 | RPC request timeout (ms) |
| `--iterator-timeout-secs` | 0 | Iterator timeout for archive mode (0 = disabled) |
| `--historical-rpc` | - | Historical RPC endpoint for pre-fork queries (e.g., OP pre-bedrock) |
| `--historical-height` | - | Fork height threshold for historical RPC forwarding |

### Kafka + S3 Configuration

When using Kafka + S3 mode, provide a JSON config file:

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

### Data Migration

Migrate initial data from Geth snapshot:

```bash
# 1. Export snapshot from Geth
./geth snapshot dump2 --dumpdb /nodex_backup --datadir /eth/state/geth/

# 2. Import to leafage-evm
RUST_LOG=info ./target/release/leafage-evm file-migrate \
  --source-path /nodex_backup \
  --db-path /path/to/leafage/db
```

## Benchmark

`leafage-bench` is a CLI tool for benchmarking `eth_call` performance between leafage-evm and geth.

### Build

```bash
cargo build --release -p leafage-bench
```

### Corpus

The benchmark corpus (`bin/leafage-bench/corpus/corpus.json`) is tracked via **Git LFS**. Pull it after cloning:

```bash
git lfs pull
```

### Subcommands

#### `run` — Run the benchmark

```bash
./target/release/leafage-bench run \
  --corpus bin/leafage-bench/corpus/corpus.json \
  --target http://leafage-evm:8545 \
  --compare http://geth:8545
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `--corpus` / `-c` | - | Path to the corpus JSON file (required) |
| `--target` | - | Primary RPC endpoint URL (leafage-evm) (required) |
| `--compare` | - | Comparison RPC endpoint URL (geth) |
| `--label` | all | Only run cases with this complexity label: `L1`, `L2`, `L3` |
| `--concurrency` | 10 | Number of concurrent requests per endpoint |
| `--requests` | corpus size | Total requests per endpoint per round |
| `--rounds` | 1 | Number of benchmark rounds |
| `--seed` | - | Shuffle seed for corpus ordering |
| `--output-dir` | - | Directory for export files (`summary.json`, `verbose.json`) |
| `--verbose` | false | Write per-request details to `verbose.json` (requires `--output-dir`) |

#### `inspect` — Inspect the corpus

Print summary statistics of the corpus without running any benchmark:

```bash
./target/release/leafage-bench inspect \
  --corpus bin/leafage-bench/corpus/corpus.json
```

## Documentation

| Document | Description |
|----------|-------------|
| [Architecture.md](docs/Architecture.md) | System architecture, crate structure, key traits |
| [StateManage.md](docs/StateManage.md) | In-memory state tree, fork handling, finalization |
| [StateUpdater.md](docs/StateUpdater.md) | Kafka + S3 and HTTP update modes |
| [Database.md](docs/Database.md) | RocksDB storage layout for state and archive nodes |
| [DataSpec.md](docs/DataSpec.md) | Data format specification for state updates |
| [Deploy](docs/deploy/) | Deployment guide with Docker Compose |

## Architecture

### State Management

leafage-evm manages state using a linked-list structure:

```
Latest Block (Head)
    ↓
Block N-1 diff
    ↓
   ...
    ↓
Block N-63 diff
    ↓
Base State (RocksDB)
```

- Recent 64 block diffs are kept in memory for fast access
- State queries search from top to bottom, falling back to RocksDB
- On new block: push new diff to head, persist oldest diff to RocksDB when exceeding depth limit

### State Update

leafage-evm supports two modes for receiving state updates:

- **Kafka + S3 (Primary)**: Receives block change notifications via Kafka, fetches block info and state diffs from S3
- **HTTP (Fallback)**: Polls `trace_debankBlock` RPC from a modified Geth instance

## Project Structure

```
leafage-evm/
├── bin/leafage-evm/           # CLI entry point
├── crates/
│   ├── leafage-evm-types/     # Type definitions
│   ├── leafage-evm-storage/   # Storage layer (RocksDB/MDBX, StateTree)
│   ├── leafage-evm-rpc/       # JSON-RPC implementation
│   └── leafage-evm-chains/    # Chain-specific logic (BSC/Cosmos precompiles)
└── docs/                      # Documentation
```

## License

MIT OR Apache-2.0
