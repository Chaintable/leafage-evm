# Deployment Guide

This document describes how to deploy leafage-evm Ethereum node using Docker Compose.

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                     Docker Network (eth)                     │
│                      10.0.90.0/24                           │
│                                                              │
│  ┌─────────────┐     ┌─────────────┐     ┌───────────────┐  │
│  │   beacon    │     │    geth     │     │ leafage-evm-x │  │
│  │ (Lighthouse)│────►│ (Execution) │◄────│ (State Query) │  │
│  │   :9000     │JWT  │ :8545/:8551 │ RPC │    :8659      │  │
│  └─────────────┘     └─────────────┘     └───────────────┘  │
│                             │                    │           │
└─────────────────────────────┼────────────────────┼───────────┘
                              │                    │
                        8666 → 8545          8659 → 8659
                              ▼                    ▼
                       External Access      External Access
```

## Services

### beacon (Lighthouse)

Beacon chain client for consensus layer:

- **Image**: `sigp/lighthouse:v8.0.1`
- **Resources**: 4 CPU / 12GB memory

### geth (Execution Layer)

Modified Geth client for execution layer:

- **Image**: `public.ecr.aws/b2h7a5c4/chaintable/ethereum-writer:v1.17.3-debank-3`
- **Resources**: 4 CPU / 24GB memory
- **Port Mapping**: 8666 → 8545 (HTTP RPC)
- **Sync Mode**: full sync + archive mode
- **RPC APIs**: `net,web3,eth,admin,debug,txpool,engine,trace`

### leafage-evm-x-eth

Lightweight EVM executor for state queries:

- **Image**: `public.ecr.aws/b2h7a5c4/chaintable/leafage-evm-x:v1.2.31`
- **Typical Resources**: 4 CPU / 16GB memory
- **Port Mapping**: 8659 → 8659
- **Features**:
  - Receives block state updates from geth
  - Provides `eth_call`, `eth_estimateGas` and other query APIs

> **Scaling**: QPS scales approximately linearly with the number of CPU cores — each request is handled by an independent worker with little cross-core contention. For higher throughput, increase the CPU allocation (e.g., 8c/32G, 16c/64G) and memory roughly proportionally.

## Resource Requirements

A typical single-host production deployment requires the following resources.

### CPU & Memory

| Service | CPU | Memory |
|---------|-----|--------|
| beacon (Lighthouse) | 4 cores | 12 GB |
| geth (full sync + archive) | 4 cores | 24 GB |
| leafage-evm-x-eth | 4 cores | 16 GB |
| **Total** | **12 cores** | **52 GB** |

leafage-evm QPS scales approximately linearly with CPU cores. To serve higher query throughput, scale the `leafage-evm-x-eth` CPU allocation up (and memory roughly proportionally) — the other two services can stay at their baseline.

### Disk

SSD is strongly recommended; archive mode in particular requires sustained high random-read/write IOPS. Expected usage at current mainnet head:

| Component | Size |
|-----------|------|
| beacon + geth (archive mode) | ~850 GB |
| leafage-evm (archive mode)   | ~450 GB |
| leafage-evm (state-only mode) | ~150 GB |

Total disk footprint:
- **~1.3 TB** — geth archive + leafage archive
- **~1.0 TB** — geth archive + leafage state-only

Leave additional headroom (20–30%) for ongoing chain growth, compaction, and snapshots.

**IOPS requirement**: at least **3000 IOPS** per volume. On AWS, **EBS gp3** is a good baseline — it provisions 3000 IOPS and 125 MB/s throughput by default at any volume size, which is sufficient for a steady-state node (processing one block every ~12s).

During the bring-up window the disk is under heavier pressure than steady state, in two phases:

1. **Snapshot extraction** — downloading and `unzstd`-decompressing the ~850 GB / ~450 GB snapshots is bound by **sequential write throughput**; gp3's default 125 MB/s is the bottleneck.
2. **Catch-up after snapshot restore** — the snapshot height lags the chain head by hours to days of blocks, and the node replays them far faster than real-time, which drives **random-read/write IOPS** well above steady state.

To shorten this one-time bring-up window (or to handle heavier query load later), provision additional IOPS/throughput on gp3, or step up to `io2`.

### Network

- Stable outbound connectivity to the Ethereum P2P network (geth)
- Access to the beacon checkpoint sync URL (`https://mainnet.checkpoint.sigp.io`)
- Access to AWS S3 for the initial snapshot download (optional, speeds up first sync)

## Quick Start

Use the one-click deployment script to automate the entire setup:

```bash
cd docs/deploy

# Interactive mode — prompts for data directories and archive mode
sudo ./deploy.sh

# Or specify options directly
sudo ./deploy.sh --eth-data-dir /data/eth --leafage-data-dir /data/leafage --archive

# Skip snapshot download if you already have data
sudo ./deploy.sh --skip-snapshot
```

The script handles directory creation, JWT generation, snapshot download & extraction, and service startup.

## Prerequisites

1. **Docker** and **Docker Compose** installed
2. **AWS CLI**, **zstd**, and **openssl** installed (for snapshot download and JWT generation)
3. **Hardware**: see [Resource Requirements](#resource-requirements) for CPU, memory, and disk sizing
4. **Network**: access to the Ethereum P2P network and the beacon checkpoint sync URL

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `ETH_DATA_DIR` | `/eth` | Eth data storage directory |
| `LEAFAGE_DATA_DIR` | `/nodex-eth` | Leafage data storage directory |

### Data Directory Structure

```
${ETH_DATA_DIR}/
├── lighthouse/          # Beacon chain data
├── geth/               # Execution layer data
│   └── jwtsecret       # JWT authentication key
└── ...

${LEAFAGE_DATA_DIR}/
├── ....sst # rocksdb data file
└── ...

```


## Snapshot Download

You can download pre-synced snapshots from S3 to speed up initial deployment.

### Download via AWS CLI

```bash
# Install zstd for decompression
apt-get install -y zstd

# Geth snapshot (block 24646705, requires ~850GB free space)
aws s3 cp s3://blockchain-snapshot-backup/eth/geth-24646705.tar.zstd .
tar --use-compress-program=unzstd -xf geth-24646705.tar.zstd -C ${ETH_DATA_DIR}/
```

Leafage snapshot has two modes, **choose one** based on your needs:

**Option A**: State mode (requires ~150GB free space)

```bash
aws s3 cp s3://blockchain-snapshot-backup/eth/leafage-24647777.tar.zstd .
tar --use-compress-program=unzstd -xf leafage-24647777.tar.zstd -C ${LEAFAGE_DATA_DIR}/
```

**Option B**: Archive mode (requires ~450GB free space, requires `--archive` flag)

```bash
aws s3 cp s3://blockchain-snapshot-backup/eth/leafage-archive-24646705.tar.zstd .
tar --use-compress-program=unzstd -xf leafage-archive-24646705.tar.zstd -C ${LEAFAGE_DATA_DIR}/
```

> **About Geth snapshot**: The provided geth snapshot is ancient-pruned. You can also use a regular full sync geth snapshot from other sources (e.g., publicnode), as long as the geth snapshot block height is **lower than or equal to** the leafage snapshot block height. Leafage syncs state from geth on startup — if geth is ahead, leafage will miss intermediate blocks and fail to sync correctly.
>
> **Note**: Archive mode requires adding the `--archive` flag to the leafage-evm startup parameters. See [leafage-evm Configuration Parameters](#leafage-evm-configuration-parameters) for details.

## Deployment Steps

### 1. Prepare Configuration

```bash
# Set data directories
export ETH_DATA_DIR=/eth
export LEAFAGE_DATA_DIR=/nodex-eth

# Create required directories
mkdir -p ${ETH_DATA_DIR}/geth ${ETH_DATA_DIR}/lighthouse ${LEAFAGE_DATA_DIR}

# Generate JWT secret
openssl rand -hex 32 > ${ETH_DATA_DIR}/geth/jwtsecret
```

### 2. Start Services

```bash
cd docs/deploy

# Start all services
docker compose up -d

# View logs
docker compose logs -f
```

### 3. Verify Service Status

```bash
# Check geth sync status
curl -X POST http://localhost:8666 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}'

# Check leafage-evm status
curl -X POST http://localhost:8659 \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
```

## Operations

### Check Service Status

```bash
docker compose ps
```

### View Logs

```bash
# All services
docker compose logs -f

# Single service
docker compose logs -f geth
docker compose logs -f beacon
docker compose logs -f leafage-evm-x-eth
```

### Restart Services

```bash
docker compose restart <service_name>
```

### Stop Services

```bash
docker compose down
```

## leafage-evm Configuration Parameters

Main startup parameters for leafage-evm-x-eth:

| Parameter | Description |
|-----------|-------------|
| `--db-path` | Database storage path |
| `--listen-addr` | Service listen address |
| `--chain-cfg` | Chain config ID (1 = Ethereum mainnet) |
| `--rpc-addr` | Upstream RPC address (geth) |
| `--archive` | Enable archive mode (required when using archive snapshot) |

## Notes

1. **Initial Sync**: First sync may take a long time; beacon uses checkpoint sync to speed up
2. **Disk I/O**: SSD recommended; archive mode requires high disk performance
3. **Memory Usage**: Geth may exceed limits under high load; adjust as needed
4. **Network Isolation**: All services communicate within isolated Docker network

## Related Documentation

- [Architecture.md](../Architecture.md) - System architecture
- [StateManage.md](../StateManage.md) - In-memory state tree and fork handling
- [StateUpdater.md](../StateUpdater.md) - State update mechanism
- [Database.md](../Database.md) - Database storage format
- [DataSpec.md](../DataSpec.md) - Data format specification
