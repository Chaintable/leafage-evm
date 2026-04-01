# Leafage

Scalable, lightweight, and modular infrastructure for EVM state queries and block data distribution.

## Overview

Leafage is a **purpose-built EVM node architecture for massive-scale products**. It is a distributed system that **decouples EVM state querying from block synchronization** and **records complete block execution data for external consumption** — designed from the ground up for teams that need to serve millions of state queries per day across multiple chains, and for data platforms that need structured access to transactions, call traces, and event logs without running their own full nodes.

Instead of querying a monolithic full node that handles consensus, execution, P2P networking, and RPC all in one process, Leafage splits the pipeline: one full node executes blocks and exports execution data, which is then distributed via Kafka + S3 to two classes of consumers. Lightweight query nodes (leafage-evm) receive state diffs for RPC serving. External analytics platforms and business applications receive structured block data — transactions, call traces, and event logs — as JSON+gzip files on S3, ready for ingestion into data warehouses and big-data pipelines.

The result: horizontally scalable EVM state queries with ~90GB storage per node instead of 1.3TB+, minute-level cold start, zero interference between block sync and RPC workloads, and a built-in data distribution layer that makes complete block execution data accessible to any downstream system.

### The problem: scaling Geth doesn't scale

The standard approach to scaling EVM state queries is straightforward — run more Geth nodes. But at production scale, this approach breaks down in fundamental ways:

**Massive resource waste.** Each Geth full node requires 1.3TB+ of storage on Ethereum mainnet, with Archive nodes reaching 2–6.5TB. Scaling to 100+ nodes means provisioning 130TB+ of largely redundant data — every node stores the same blocks, transactions, receipts, and state. CPU and memory costs compound similarly: every node independently executes every block (CPU-intensive EVM computation), maintains its own Merkle Patricia Trie (memory-intensive state tree), and runs P2P networking and transaction pool management — all of which are irrelevant to serving RPC queries.

**Bandwidth explosion.** Every Geth node participates in P2P gossip, independently discovering and downloading blocks from the network. At 100+ nodes, the aggregate P2P bandwidth consumption becomes enormous — the same block data is fetched over the network hundreds of times. This not only wastes bandwidth but can also cause issues with P2P peer limits and network congestion.

**State inconsistency across replicas.** P2P synchronization is inherently non-deterministic. Different nodes receive blocks at different times, process reorgs independently, and may temporarily sit at different chain heights. For applications that query multiple nodes behind a load balancer, this means the same `eth_call` can return different results depending on which node handles the request — a critical problem for DeFi protocols and data platforms that require consistent reads.

**Coupled workloads with mutual interference.** In a monolithic Geth node, block synchronization (CPU-heavy EVM execution + disk-heavy state writes) and RPC query serving share the same process, memory, and disk I/O. A spike in query traffic slows down sync; a heavy sync period degrades query latency. There is no way to independently scale or isolate these workloads.

**Slow recovery and scaling.** When a Geth node fails, replacing it requires hours to days of chain resync or snapshot import. Scaling out to handle traffic spikes is equally slow — you cannot spin up a new full node in minutes. This makes the fleet brittle and unresponsive to demand changes.

**No built-in data export for analytics.** Geth has no pipeline for delivering structured block data to external systems. Extracting transactions, call traces, and event logs at scale requires calling expensive RPCs like `debug_traceBlock` — which contend with production query traffic and do not scale horizontally. Teams that need this data for analytics, compliance, or business intelligence end up building and maintaining custom ETL infrastructure on top of an API that was never designed for bulk export.

These are the problems Leafage was built to solve — replacing the "scale by cloning full nodes" model with a distributed architecture that eliminates redundancy, guarantees consistency, scales query capacity independently of execution, and provides a built-in data distribution layer for external consumers.

### What is Leafage?

Leafage answers the problems above by splitting the monolithic full node into a distributed pipeline. Instead of every node independently syncing, storing, and serving the same data, Leafage uses **one** full node for execution and exports the results to **any number** of lightweight, specialized consumers — eliminating redundant storage, guaranteeing cross-replica consistency via Kafka, isolating sync from query workloads, enabling minute-level recovery from S3 snapshots, and providing a built-in data export layer for analytics.

It is a collection of five components that together form a complete EVM state query and block data distribution stack:

| Component | Language | Role |
|-----------|----------|------|
| **go-ethereum-x** | Go | Modified Geth that exports state diffs during block execution |
| **pipeline** | Go | Serializes and distributes execution data via Kafka + S3: state diffs to leafage-evm, block data (txs/traces/events) to external consumers |
| **consistency_checker** | Go | Validates block consistency across replicas and publishes confirmed notifications to external consumers |
| **leafage-evm** | Rust | Lightweight EVM executor that consumes state diffs and serves RPC queries |
| **nodex-proxy** | Go | JSON-RPC gateway with service discovery, load balancing, and smart routing |

The project is licensed under MIT OR Apache-2.0.

### Goals

**Horizontal scalability.** Adding query capacity should be as simple as launching a new leafage-evm instance — no full chain resync, no 1TB+ disk provisioning, no hours of waiting. Only one full node syncs via P2P; all query nodes consume from Kafka + S3, eliminating the redundant bandwidth of 100+ nodes each pulling the same blocks from the network.

**Resource isolation.** Block synchronization (CPU-intensive EVM execution + disk-intensive state writes) and RPC queries should never compete for resources. Leafage runs them in separate processes, on separate machines if needed — a query traffic spike will never slow down sync, and vice versa.

**Cross-replica consistency.** All query nodes consume from the same Kafka stream and apply the same blocks in the same order. No P2P non-determinism, no height divergence behind a load balancer — the same `eth_call` returns the same result regardless of which node handles it.

**Lightweight resource footprint.** leafage-evm does not execute blocks, does not maintain a Merkle Patricia Trie, and does not run P2P networking — eliminating the CPU, memory, and disk overhead that dominates a Geth full node. Query nodes only need account state (balance, nonce, code, storage); transactions, receipts, and logs are irrelevant for `eth_call` and can be discarded — reducing storage from 1.3TB+ to 90GB, with proportionally lower CPU and memory requirements.

**Fast recovery.** A failed node should be replaceable in minutes (S3 snapshot + Kafka catch-up), not hours (P2P resync).

**Built-in data distribution.** Pipeline records complete block execution data (transactions, call traces, event logs) and uploads it to S3 as structured JSON+gzip — no custom ETL pipelines, no expensive `debug_traceBlock` calls contending with production traffic.

**Multi-chain with a unified stack.** The same architecture, deployment tools, and monitoring system covers Ethereum, Optimism, BSC, Cosmos EVM, and Mantle. Switch chains with a single `--evm-type` flag.

### Who is this for?

**DeFi protocols and wallets** that need high-throughput `eth_call`, `eth_estimateGas`, and batch contract calls across multiple chains.

**Data platforms and analytics teams** that need structured access to block execution data — transactions, call traces, and event logs — without running full nodes or maintaining custom ETL pipelines. Pipeline's external S3 bucket delivers this data as JSON+gzip files ready for warehouse ingestion.

**Infrastructure teams** managing multi-chain deployments that want a unified, observable architecture instead of maintaining separate full nodes for each chain.

If your bottleneck is EVM state query throughput, or you need reliable access to structured block data at scale, Leafage is for you.

## Architecture

```
                          ┌───────────┐          ┌─────────────────────┐
                          │  Clients  │          │ External Consumers  │
                          └─────┬─────┘          └──────────┬──────────┘
                                │ JSON-RPC                  │ subscribe
                                ▼                           ▼
                       ┌─────────────────┐      ┌──────────────────────────┐
                       │   nodex-proxy   │      │   consistency_checker    │
                       │   (RPC Gateway) │      │ (validation & fork coord)│
                       └────────┬────────┘      └──────────────────────────┘
                  etcd service  │  weighted              ▲ poll eth_blockNumber
                  discovery     │  routing               │
                  ┌─────────────┼─────────────┐          │
                  ▼             ▼             ▼          │
           ┌────────────┐┌────────────┐┌────────────┐    │
           │leafage-evm ││leafage-evm ││leafage-evm │────┘
           │(State Node)││(Archive)   ││(State Node)│
           └─────┬──────┘└─────┬──────┘└─────┬──────┘
                 └─────────────┼─────────────┘
                               │ consume Kafka + S3
                               ▼
                 ┌──────────────────────────┐
                 │        pipeline          │
                 │   (Kafka + S3 transport) │
                 └────────────┬─────────────┘
                              │ EVM tracing hooks
                              ▼
                 ┌──────────────────────────┐
                 │     go-ethereum-x        │
                 │  (Geth fork + Tracer)    │
                 └────────────┬─────────────┘
                              ▲ P2P sync
                         Ethereum Network
```

### Data flow: from block to query

A new block goes through five stages before it becomes queryable. Separately, consistency_checker validates replica convergence and notifies external consumers.

```
┌─ 1. Execution ────────────────────────────────────────────────────────────┐
│  go-ethereum-x receives a block via P2P and executes it.                 │
│  Pipeline Tracer hooks fire during execution, capturing state diffs,     │
│  call traces, and event logs.                                            │
└──────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─ 2. Distribution ─────────────────────────────────────────────────────────┐
│  Pipeline serializes the data and uploads:                               │
│  • Header + StateDiff → S3 (internal bucket)                             │
│  • BlockFile (txs/traces/events) → S3 (external bucket)                  │
│  • BlockChangeNotification → Kafka                                       │
└──────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─ 3. State ingestion ──────────────────────────────────────────────────────┐
│  leafage-evm consumes the Kafka notification directly, fetches Header    │
│  and StateDiff from S3, and applies the diff to its in-memory StateTree. │
└──────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─ 4. Finalization ─────────────────────────────────────────────────────────┐
│  When a block's depth exceeds 64, its state is persisted from the        │
│  in-memory diff tree to RocksDB. The in-memory layer is released.        │
└──────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─ 5. Query serving ────────────────────────────────────────────────────────┐
│  Client → nodex-proxy → leafage-evm.                                     │
│  The proxy routes to the right node type (State/Archive) based on the    │
│  requested block context. leafage-evm executes eth_call against its      │
│  local state and returns the result.                                     │
└──────────────────────────────────────────────────────────────────────────┘

┌─ Parallel: Consistency validation ────────────────────────────────────────┐
│  consistency_checker also consumes the Kafka notification. It polls all   │
│  leafage-evm replicas via eth_blockNumber — only after ≥80% have synced  │
│  to the new height does it publish a confirmed notification to the       │
│  external Kafka topic. External consumers (not leafage-evm) subscribe    │
│  to this topic to receive consistency-guaranteed block updates.           │
└──────────────────────────────────────────────────────────────────────────┘
```

## Core Components

### go-ethereum-x

A fork of go-ethereum v1.16.8 with two additions that make it a data source for the Leafage pipeline:

**Pipeline Live Tracer.** A `tracing.Hooks` implementation that fires during EVM execution. It captures balance/nonce/storage/code changes, call traces, and event logs in real time, then hands them to Pipeline for upload. Enabled via:

```bash
geth --vmtrace pipeline --vmtrace.jsonconfig '{"kafka_brokers":"...", "s3_bucket":"..."}'
```

**`trace_debankBlock` RPC.** Returns the full execution output of a block (header + state diff + traces + events) in a single RPC call. Used as an HTTP fallback when Kafka is unavailable — leafage-evm can poll this endpoint directly.

**Custom tracing hooks** added to go-ethereum's `core/tracing`:

| Hook | Trigger | Purpose |
|------|---------|---------|
| `OnCommit` | After state commit | Exports the state diff (modified accounts, storage, code) |
| `OnGenesisBlock` | Genesis processing | Generates synthetic txs/traces for initial allocations |
| `OnBlockDBStart` | Before block execution | Provides StateDB access to the tracer |

### Pipeline

A Go library embedded in go-ethereum-x. It serializes execution data and distributes it to downstream consumers.

**Dual-bucket S3 strategy:**

| Bucket | Data | Format | Consumer |
|--------|------|--------|----------|
| Internal (NodeX) | Header, StateDiff | JSON+gzip, RLP | leafage-evm |
| External (ChainTable) | BlockFile, Validation | JSON+gzip | Business applications |

**High availability** via etcd leader election — multiple Geth instances can run Pipeline, but only the leader publishes to Kafka. All instances upload to S3.

**Two integration modes:**

- **Live Tracer** — hooks into EVM execution in real time. Zero latency, requires modified execution client.
- **RPC Tracer** — calls `trace_debankBlock` on demand. Works with any compatible Geth fork without core modifications.

### Consistency Checker

An independent validation layer that runs alongside the main data pipeline. It does **not** sit in the data path between Pipeline and leafage-evm — instead, it observes both and provides a consistency-guaranteed notification stream for external consumers.

**How it works:**

1. **Consume.** Subscribes to the same Kafka topic as leafage-evm (Pipeline's block change notifications).
2. **Poll.** For each new block, polls all leafage-evm replicas via `eth_blockNumber` RPC. Waits until ≥80% (configurable) of replicas have synced to the target height.
3. **Validate.** Checks for forks by listing all blocks at the same height in S3. If multiple hashes exist, marks non-canonical blocks as forks.
4. **Publish.** Only after replicas have converged, publishes a confirmed `OuterBlockChangeNotification` to the external Kafka topic.

External consumers (analytics platforms, indexers, business applications — not leafage-evm) subscribe to this external topic. They are guaranteed that every notification they receive has already been applied and verified across the leafage-evm cluster.

**Topic alignment** — in multi-version deployments, the leader (elected via etcd distributed lock) ensures the version-specific topic and the singleton topic stay in sync by fast-forwarding or rolling back to a common ancestor.

**Coordination stack:** etcd (leader election + node registry), Kafka (message transport), S3 (block validation storage), PebbleDB (local dual-index for block lookup).

### leafage-evm

A Rust EVM executor built on revm and alloy. It does not participate in P2P consensus — it consumes block notifications directly from Pipeline's Kafka topic, fetches state diffs from S3, and serves RPC queries.

**State management:**

```
Recent 64 blocks (in-memory)                          Finalized state (on-disk)

Block N ──► Block N-1 ──► ... ──► Block N-63 ──►  CacheDiskLayer ──► RocksDB
(DiffLayer)  (DiffLayer)          (DiffLayer)       (read cache)
```

- **In-memory diff tree.** The latest 64 blocks are stored as a linked list of `DiffLayer` nodes. Each layer holds only the state changes (diff) relative to its parent. Queries walk the chain from newest to oldest until the key is found.
- **Fork support.** Fork blocks exist in `hash_diff_map` but not in `num_diff_map` (which tracks only the canonical chain). Queries by block hash can access fork states.
- **Disk persistence.** When a block's depth exceeds 64, its accumulated state is flushed to RocksDB/MDBX.

**Two node types:**

| Type | Storage (ETH mainnet) | State range | Use case |
|------|----------------------|-------------|----------|
| **State Node** | ~90 GB | Latest only | Most RPC queries |
| **Archive Node** | ~360 GB | Full history | Historical `eth_call` at arbitrary blocks |

Archive nodes use a dual-write strategy: `address || block_num` for historical lookups (via RocksDB `seek_for_prev`) and `address || u64::MAX` for latest-state fast path.

**State update modes:**

- **Kafka + S3** (production) — consumes `BlockChangeNotification` from Kafka, fetches Header and StateDiff from S3 in parallel, applies to StateTree.
- **HTTP polling** (development/fallback) — polls `trace_debankBlock` RPC on go-ethereum-x, handles reorgs by walking back to the common ancestor.

**Service registration.** On startup, leafage-evm registers itself in etcd at `{chain_id}/nodes/{ip}_{port}` with metadata (node type, state type). nodex-proxy discovers it automatically.

### nodex-proxy

A JSON-RPC gateway built on Cloudwego Hertz. It abstracts the leafage-evm cluster into a single RPC endpoint.

**Service discovery.** Watches etcd for node registrations. When a leafage-evm instance starts or stops, the proxy updates its pool in real time. New nodes are health-checked (via `getLatestBlock` RPC) before being added.

**Smart routing.** Inspects the block parameter in each RPC request:

- Latest / pending / within 64 blocks → **State Node**
- Older than 64 blocks → **Archive Node**
- Cosmos precompile call → **Native Node**

**Load balancing.** Two strategies, configurable per chain:

- **Random Weighted** — probabilistic selection based on node weights
- **Round-Robin Weighted** — deterministic rotation with weight-adjusted frequency

**Automatic failover:**

- `StateBlockNotFound` (-39006) → retry on Archive Node
- `CosmosPrecompile` (-39008) → retry on Native Node

**Additional features:**

- Per-method RPS rate limiting
- Request mirroring to shadow backends (async, for traffic analysis or canary testing)
- Dynamic weight adjustment via HTTP admin API
- Method-level routing rules (include/exclude lists per node)

## Supported Chains

leafage-evm supports multiple EVM-compatible chains via the `--evm-type` flag:

| Chain | Flag | Highlights |
|-------|------|-----------|
| **Ethereum** | `mainnet` | Standard EVM, EIP-2935 blockhashes contract |
| **Optimism** | `op` | L2 gas calculation, OVM precompiles, pre-bedrock RPC forwarding |
| **BSC** | `bsc` | Parlia validator blacklist, Tendermint/IAVL precompiles |
| **Cosmos EVM** | `cosmos` | bech32 addresses, p256 signature verification, native token handling |
| **Mantle v2** | `mantlev2` | OP Stack based |

Adding a new chain requires implementing the `EvmExecutor` trait — chain-specific precompiles, hardfork rules, and gas calculations are encapsulated per chain in the `leafage-evm-chains` crate.

## RPC Interface

### Standard Ethereum

| Method | Notes |
|--------|-------|
| `eth_call` | Execute message call |
| `eth_estimateGas` | Estimate gas for a transaction |
| `eth_getBalance` | Account balance |
| `eth_getCode` | Contract bytecode |
| `eth_getStorageAt` | Storage slot value |
| `eth_getTransactionCount` | Account nonce |
| `eth_blockNumber` | Latest block number |
| `eth_getBlockByNumber` / `eth_getBlockByHash` | Block header only (no transaction bodies) |
| `eth_chainId` | Chain ID |
| `eth_baseFee` | Current base fee |

### Extended

| Method | Description |
|--------|-------------|
| `eth_multiCall` | Batch execute multiple calls in parallel, with `fast_fail` and cache control |
| `contractMultiCall` | Batch contract calls with `BlockOverrides` support |
| `simulateTransactions` | Simulate a sequence of transactions and predict results |
| `estimateGas` | Enhanced gas estimation |
| `pre_traceCall` | Single call trace |
| `pre_traceMany` | Batch call traces |
| `getLatestBlock` / `getBlockByHeight` / `getBlockById` | Block info queries |
| `blockIsValid` | Check if a block is on the canonical chain |

## Why Leafage over monolithic Geth?

### Horizontal scalability

| | Monolithic Geth | Leafage |
|---|---|---|
| Scale-up method | Each instance syncs the full chain independently | Add leafage-evm instances |
| Cost per instance | 1.3TB+ disk, hours of sync | 90GB disk, minutes (S3 snapshot) |
| Scaling ceiling | Limited by P2P network and disk I/O | Kafka + S3 throughput (practically unlimited) |

With Geth, 10x query capacity means 10x full nodes, each with 1.3TB of redundant data. With Leafage, it means 10x lightweight query nodes that share a single data pipeline.

### Resource isolation

In monolithic Geth, block sync and RPC queries share CPU, memory, and disk I/O. High query load slows down sync; sync spikes increase query latency.

Leafage separates them:
- **go-ethereum-x** — dedicated to sync and execution
- **leafage-evm** — dedicated to queries

They run in different processes, on different machines, with independent resource budgets.

### Storage efficiency — an order of magnitude smaller Archive

| | Geth Full | Geth Archive (flat state) | Geth Archive (+ trie) | leafage-evm State | leafage-evm Archive |
|---|---|---|---|---|---|
| ETH mainnet | ~900GB | **~2TB** | **~6.5TB** | **~90GB** | **~360GB** |
| Stored data | Blocks, txs, receipts, latest state | + full flat state history | + historical trie data | State only | State + history |

The comparison is starkest for Archive nodes. A Geth Archive node with full flat state history requires **~2TB** on Ethereum mainnet (or ~6.5TB if historical trie data is also retained). leafage-evm Archive stores only account state history (balance, nonce, code, storage) at **~360GB** — roughly **5–18x smaller** depending on the Geth configuration.

This isn't a compression trick — it's a fundamental architectural difference. leafage-evm discards everything irrelevant to `eth_call`: transaction bodies, receipts, call traces, event logs, and trie nodes. What remains is the minimal dataset needed to execute state queries at any historical block.

### High performance — RocksDB + revm

leafage-evm is built for raw query throughput. The performance advantage comes from the combination of its storage engine and execution runtime:

**RocksDB with purpose-built column families.** State is stored in a flat key-value layout — no Merkle Patricia Trie traversal. Account lookups are direct `db.get()` operations with O(1) complexity. Archive historical queries use RocksDB's `seek_for_prev` with prefix extractors tuned per column family (32-byte prefix for accounts, 64-byte for storage), keeping iterator seeks fast even across billions of keys.

**revm — the fastest EVM implementation.** leafage-evm uses revm (Rust EVM), the same execution engine powering Reth, Foundry, and other performance-critical Ethereum tooling. Combined with Rust's zero-cost abstractions and alloy's optimized type system, `eth_call` execution avoids the overhead of Go's garbage collector and runtime that Geth carries.

**No trie overhead.** Geth resolves every state access through a Merkle Patricia Trie — multiple LevelDB lookups per account or storage slot. leafage-evm bypasses this entirely: state is stored flat, and the in-memory diff tree for recent blocks means most queries never touch disk at all.

**In-memory hot path.** The latest 64 blocks live in the in-memory diff tree. For the overwhelming majority of RPC queries (which target `latest` or near-latest blocks), state resolution is a pure memory walk — no disk I/O, no deserialization overhead.

### Multi-chain unified operations

Without Leafage, each chain requires its own full node implementation (Geth, op-node, bsc-node, etc.) with different deployment, monitoring, and upgrade workflows.

With Leafage:
- **Data source:** go-ethereum-x + pipeline (one per chain)
- **Query nodes:** leafage-evm with `--evm-type=mainnet|op|bsc|cosmos|mantlev2`
- **Gateway:** nodex-proxy routes by `chainId`
- **Monitoring:** same Prometheus metrics, same Grafana dashboards, all chains

One stack. One set of runbooks. All chains.

### Cold start and recovery

| | Monolithic Geth | Leafage |
|---|---|---|
| Cold start | Sync from genesis or import snapshot (hours to days) | Download RocksDB snapshot from S3 (minutes), catch up from Kafka offset |
| Failure recovery | Resync or restore from backup | New instance pulls snapshot + catches up on Kafka, auto-registers in etcd |
| Scale-out response time | Slow (full sync required) | Fast (snapshot + incremental) |

### Enhanced query capabilities

leafage-evm provides business-optimized RPC methods beyond the standard `eth_*` interface:

- **`contractMultiCall`** — batch multiple contract calls in a single request, with block parameter overrides
- **`simulateTransactions`** — simulate transaction sequences and predict execution results
- **`eth_multiCall`** — parallel batch calls with `fast_fail` and cache control
- **`pre_traceCall` / `pre_traceMany`** — call tracing without the full node's debug API overhead

These methods either don't exist in standard Geth or require additional middleware to implement.

### Summary

| Dimension | Monolithic Geth | Leafage |
|-----------|----------------|---------|
| Scaling model | Vertical (bigger machines) | Horizontal (more instances) |
| Archive storage | ~2TB – 6.5TB (ETH mainnet) | ~360GB (5–18x smaller) |
| State storage | ~900GB | ~90GB |
| State access | MPT traversal (multiple disk reads) | Flat KV lookup, O(1) |
| EVM runtime | Go (GC pauses, runtime overhead) | revm / Rust (zero-cost abstractions) |
| Hot-path queries | Always hits disk (LevelDB) | In-memory diff tree for latest 64 blocks |
| Query vs. sync | Coupled, mutual interference | Isolated, independently scalable |
| Multi-chain ops | One full node stack per chain | Unified architecture, config switch |
| Cold start | Hours to days | Minutes |
| Failure recovery | Slow, P2P dependent | Fast, S3 snapshot based |
| Custom RPC | Modify Geth source (Go) | Native Rust implementation, independent iteration |

## Get Started

- **[Architecture](./Architecture.md)** — Detailed crate structure and module design of leafage-evm
- **[State Management](./StateManage.md)** — In-memory diff tree, fork handling, and disk persistence
- **[State Updater](./StateUpdater.md)** — Kafka + S3 and HTTP update modes
- **[Database](./Database.md)** — RocksDB column families, State vs Archive layout
- **[Data Spec](./DataSpec.md)** — Wire formats for BlockStorageDiff, S3 objects, and Kafka messages
- **[Deployment](./deploy/Deploy.md)** — Docker Compose setup with Beacon, Geth, and leafage-evm
