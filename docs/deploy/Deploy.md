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
                         Port 8666            Port 8659
                              ▼                    ▼
                       External Access      External Access
```

## Services

### beacon (Lighthouse)

Beacon chain client for consensus layer:

- **Image**: `sigp/lighthouse:v8.0.1`
- **Resources**: 4 CPU / 12GB memory
- **Features**:
  - Syncs Ethereum consensus layer
  - Communicates with geth via JWT authentication
  - Uses checkpoint sync for faster initial sync

### geth (Execution Layer)

Modified Geth client for execution layer:

- **Image**: `294354037686.dkr.ecr.ap-northeast-1.amazonaws.com/blockchain-gethx:amd64-v1.16.7-debank-4`
- **Resources**: 4 CPU / 24GB memory
- **Port Mapping**: 8666 → 8545 (HTTP RPC)
- **Sync Mode**: full sync + archive mode
- **RPC APIs**: `net,web3,eth,admin,debug,txpool,pre,engine,trace`

### leafage-evm-x-eth

Lightweight EVM executor for state queries:

- **Image**: `294354037686.dkr.ecr.ap-northeast-1.amazonaws.com/leafage-evm-x:amd64-chaintable-v102-debank-7`
- **Port Mapping**: 8659 → 8659
- **Features**:
  - Receives block state updates from geth
  - Provides `eth_call`, `eth_estimateGas` and other query APIs

## Prerequisites

1. **Docker** and **Docker Compose** installed
2. **Storage**:
   - Geth archive mode: ~2TB+
   - Lighthouse: ~200GB
   - leafage-evm: ~360GB (archive) or ~90GB (state)
3. **Network**: Access to Ethereum P2P network and checkpoint sync URL

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
# Set data directory
export DATA_DIR=/eth

# Create required directories
mkdir -p ${DATA_DIR}/geth ${DATA_DIR}/lighthouse

# Generate JWT secret
openssl rand -hex 32 > ${DATA_DIR}/geth/jwtsecret
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
