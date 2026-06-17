# leafage-evm

[English](README.md)

leafage-evm 是一个基于 [alloy](https://github.com/alloy-rs/alloy) 和 [revm](https://github.com/bluealloy/revm) 实现的轻量级 EVM 执行器。专注于**状态查询**（`eth_call`、`eth_estimateGas` 等），**不存储交易数据**。通过 Kafka + S3 获取状态更新，而非 P2P 同步。

## 特性

- **两种节点模式**：
  - **State 节点**：仅保留最近区块的状态（默认 64 个区块），ETH 主网约 90GB（截至 2025.1）
  - **Archive 节点**：保留完整历史状态，ETH 主网约 360GB（截至 2025.1）
- **多链支持**：Ethereum mainnet、Optimism、BSC、Cosmos EVM
- **多数据库后端**：RocksDB（默认）、MDBX
- **数据迁移**：支持从 Geth 快照导入初始状态

## 支持的写节点仓库

任何兼容 EVM 的链都有可能获得支持。目前已支持以下链：

| 链                                             | 仓库                                                                                               |
|-----------------------------------------------|--------------------------------------------------------------------------------------------------|
| ETH                                           | [Chaintable/go-ethereum](https://github.com/Chaintable/go-ethereum)                              |
| AVAX                                          | [Chaintable/coreth](https://github.com/Chaintable/coreth)                                        |
| OP Stack op-geth: OP, Base, opBNB, Celo, B2, BOB, DBK, Hemi, Katana, Manta, Mantle, Mode, Orderly, Soneium, Unichain, X Layer | [Chaintable/op-geth](https://github.com/Chaintable/op-geth)                                      |
| OP Stack op-reth: HSK, Ink, Lisk, Zora, Cyber | [Chaintable/optimism](https://github.com/Chaintable/optimism)                                    |
| Arb, Gravity, Plume, Hood                     | [Chaintable/nitro](https://github.com/Chaintable/nitro)                                          |
| Gnosis                                        | [Chaintable/erigon](https://github.com/Chaintable/erigon)                                        |
| Tempo                                         | [Chaintable/tempo](https://github.com/Chaintable/tempo)                                          |
| Bitlayer                                      | [Chaintable/bitlayer-l2](https://github.com/Chaintable/bitlayer-l2)                              |
| Oasys                                         | [Chaintable/oasys-validator](https://github.com/Chaintable/oasys-validator)                      |
| Kava                                          | [Chaintable/kava](https://github.com/Chaintable/kava)                                            |
| IoTeX                                         | [Chaintable/iotex-core-x](https://github.com/Chaintable/iotex-core-x)                            |
| Scrl                                          | [Chaintable/go-ethereum-scrl](https://github.com/Chaintable/go-ethereum-scrl)                    |
| Bera                                          | [Chaintable/bera-geth](https://github.com/Chaintable/bera-geth)                                  |
| Story                                         | [Chaintable/story-geth](https://github.com/Chaintable/story-geth)                                |
| Tac                                           | [Chaintable/tacchain](https://github.com/Chaintable/tacchain)                                    |
| Mitosis                                       | [Chaintable/reth-mitosis](https://github.com/Chaintable/reth-mitosis)                            |
| XDC                                           | [Chaintable/XDPoSChain](https://github.com/Chaintable/XDPoSChain)                                |
| Citrea                                        | [Chaintable/citrea](https://github.com/Chaintable/citrea)                                        |
| ZKsync: Lens, Era, Abstract, Sophon           | [Chaintable/zksync-era @ debank](https://github.com/Chaintable/zksync-era/tree/debank)           |
| Cronos zkEVM (Croze)                          | [Chaintable/zksync-era @ chain/croze](https://github.com/Chaintable/zksync-era/tree/chain/croze) |
| Fraxtal                                       | [Chaintable/frax-op-reth](https://github.com/Chaintable/frax-op-reth)                            |
| Ronin                                         | [Chaintable/conduit-op-reth](https://github.com/Chaintable/conduit-op-reth)                      |
| World Chain                                   | [Chaintable/world-chain](https://github.com/Chaintable/world-chain)                              |
| Plasma, Botanix                               | [Chaintable/reth-x](https://github.com/Chaintable/reth-x)                                        |
| BSC                                           | [Chaintable/bsc-x](https://github.com/Chaintable/bsc-x)                                          |
| Core                                          | [Chaintable/core](https://github.com/Chaintable/core)                                            |
| Chiliz                                        | [Chaintable/chiliz-chain-v2](https://github.com/Chaintable/chiliz-chain-v2)                      |
| Morph                                         | [Chaintable/go-ethereum-morph-x](https://github.com/Chaintable/go-ethereum-morph-x)              |
| Taiko                                         | [Chaintable/taiko-geth](https://github.com/Chaintable/taiko-geth)                                |
| Metis                                         | [Chaintable/mvm-x](https://github.com/Chaintable/mvm-x)                                          |
| 0G                                            | [Chaintable/0g-geth](https://github.com/Chaintable/0g-geth)                                      |
| Immutable zkEVM                               | [Chaintable/immutable-geth](https://github.com/Chaintable/immutable-geth)                        |
| Kite                                          | [Chaintable/subnet-evm-kite](https://github.com/Chaintable/subnet-evm-kite)                      |
| Merlin                                        | [Chaintable/cdk-erigon](https://github.com/Chaintable/cdk-erigon)                                |
| Flare                                         | [Chaintable/go-flare-x](https://github.com/Chaintable/go-flare-x)                                |
| Moonbeam / Moonriver                          | [Chaintable/moonbeam-x](https://github.com/Chaintable/moonbeam-x)                                |
| Conflux                                       | [Chaintable/conflux-rust-x](https://github.com/Chaintable/conflux-rust-x)                        |
| Kaia (Klaytn)                                 | [Chaintable/kaia](https://github.com/Chaintable/kaia)                                            |
| WEMIX                                         | [Chaintable/go-wemix](https://github.com/Chaintable/go-wemix)                                    |
| Polygon PoS                                   | [Chaintable/bor](https://github.com/Chaintable/bor)                                              |
| Sonic                                         | [Chaintable/sonic](https://github.com/Chaintable/sonic)                                          |
| Blast                                         | [Chaintable/blast](https://github.com/Chaintable/blast)                                          |

## 支持的 JSON-RPC 方法

### eth_*

| 方法 | 说明 |
|------|------|
| `eth_call` | 执行合约调用 |
| `eth_multiCall` | 批量执行合约调用 |
| `eth_blockNumber` | 获取当前区块高度 |
| `eth_getBalance` | 获取账户余额 |
| `eth_getBlockByNumber` | 按高度获取区块 |
| `eth_getBlockByHash` | 按哈希获取区块 |
| `eth_getCode` | 获取合约代码 |
| `eth_getStorageAt` | 获取存储槽数据 |
| `eth_getTransactionCount` | 获取账户 nonce |
| `eth_chainId` | 获取链 ID |
| `eth_baseFee` | 获取基础费用 |

### DeBankApi（无命名空间前缀）

| 方法 | 说明 |
|------|------|
| `version` | 获取版本信息 |
| `getAddressNonce` | 获取账户 nonce |
| `getAddressBalance` | 获取账户余额 |
| `getAddressCode` | 获取合约代码 |
| `getStorageAt` | 获取存储槽数据 |
| `contractMultiCall` | 批量合约调用 |
| `simulateTransactions` | 模拟交易执行 |
| `estimateGas` | 估算 Gas |
| `getLatestBlock` | 获取最新区块 |
| `getBlockByHeight` | 按高度获取区块 |
| `getBlockById` | 按哈希获取区块 |
| `blockIsValid` | 校验区块有效性 |

> **注意**：区块查询方法（`eth_getBlockByNumber`、`eth_getBlockByHash`、`getLatestBlock`、`getBlockByHeight`、`getBlockById`）**仅返回 header**，`transactions` 和 `uncles` 始终为空。leafage-evm 不存储交易数据。

### pre_*

| 方法 | 说明 |
|------|------|
| `pre_traceCall` | 预执行调用追踪 |
| `pre_traceMany` | 批量预执行追踪 |

## 构建

**环境要求**：Rust 1.79+

```bash
cargo build --release
```

Docker 构建：

```bash
docker build -t leafage-evm .
```

## 运行

### 启动服务

```bash
RUST_LOG=info ./target/release/leafage-evm standalone \
  --db-path /path/to/db \
  --listen-addr 0.0.0.0:8545 \
  --rpc-addr http://geth:8545 \
  --evm-type mainnet \
  --chain-cfg 1
```

### 主要参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--db-path` | - | 数据库路径（必需） |
| `--listen-addr` | 0.0.0.0:8545 | RPC 监听地址 |
| `--rpc-addr` | - | Geth RPC 地址（用于 HTTP 模式状态更新） |
| `--evm-type` | mainnet | EVM 类型：mainnet/op/bsc/cosmos |
| `--chain-cfg` | 1 | 链 ID |
| `--db-type` | rocksdb | 数据库类型：rocksdb/mdbx |
| `--db-cache` | 2048 | 数据库缓存大小（MB） |
| `--diff-depth-limit` | 64 | 内存中保留的区块差异深度 |
| `--archive` | false | 启用归档模式 |
| `--prometheus-addr` | - | Prometheus 监控地址 |
| `--kafka-s3-config` | - | Kafka + S3 配置文件路径 |
| `--max-connections` | 5000 | 最大并发 RPC 连接数 |
| `--rpc-timeout` | 10000 | RPC 请求超时时间（毫秒） |
| `--iterator-timeout-secs` | 0 | 归档模式迭代器超时（0 = 禁用） |
| `--historical-rpc` | - | 历史 RPC 端点，用于无法获取 block diff 的区块（如 OP pre-bedrock） |
| `--historical-height` | - | 历史 RPC 转发的分叉高度阈值 |

### Kafka + S3 配置

使用 Kafka + S3 模式时，需提供 JSON 配置文件：

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

### 数据迁移

从 Geth 快照迁移初始数据：

```bash
# 1. 在 Geth 端导出快照
./geth snapshot dump2 --dumpdb /nodex_backup --datadir /eth/state/geth/

# 2. 导入到 leafage-evm
RUST_LOG=info ./target/release/leafage-evm file-migrate \
  --source-path /nodex_backup \
  --db-path /path/to/leafage/db
```

## 性能测试

`leafage-bench` 是用于对比 leafage-evm 与 geth 的 `eth_call` 性能的 CLI 工具。

### 构建

```bash
cargo build --release -p leafage-bench
```

### 测试语料库

测试语料库（`bin/leafage-bench/corpus/corpus.json`）通过 **Git LFS** 管理，克隆仓库后执行：

```bash
git lfs pull
```

### 子命令

#### `run` — 执行性能测试

```bash
./target/release/leafage-bench run \
  --corpus bin/leafage-bench/corpus/corpus.json \
  --target http://leafage-evm:8545 \
  --compare http://geth:8545
```

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--corpus` / `-c` | - | 语料库 JSON 文件路径（必需） |
| `--target` | - | 主测目标 RPC 地址（leafage-evm）（必需） |
| `--compare` | - | 对比 RPC 地址（geth） |
| `--label` | all | 仅运行指定复杂度的用例：`L1`、`L2`、`L3` |
| `--concurrency` | 10 | 每个端点的并发请求数 |
| `--requests` | 语料库大小 | 每轮每个端点发送的总请求数 |
| `--rounds` | 1 | 测试轮数 |
| `--seed` | - | 语料库随机排序种子 |
| `--output-dir` | - | 输出目录（生成 `summary.json`、`verbose.json`） |
| `--verbose` | false | 将每条请求详情写入 `verbose.json`（需配合 `--output-dir` 使用） |

#### `inspect` — 查看语料库信息

不执行测试，仅输出语料库的统计摘要：

```bash
./target/release/leafage-bench inspect \
  --corpus bin/leafage-bench/corpus/corpus.json
```

## 文档

| 文档 | 说明 |
|------|------|
| [Architecture.md](docs/Architecture.md) | 系统架构、crate 结构、核心 trait |
| [StateManage.md](docs/StateManage.md) | 内存状态树、fork 处理、区块落盘 |
| [StateUpdater.md](docs/StateUpdater.md) | Kafka + S3 和 HTTP 更新模式 |
| [Database.md](docs/Database.md) | RocksDB 存储布局（State/Archive 节点） |
| [DataSpec.md](docs/DataSpec.md) | 状态更新数据格式规范 |
| [Deploy](docs/deploy/) | Docker Compose 部署指南 |

## 架构

### 状态管理

leafage-evm 使用链表结构管理状态：

```
最新区块 (Head)
    ↓
区块 N-1 差异
    ↓
   ...
    ↓
区块 N-63 差异
    ↓
基础状态 (RocksDB)
```

- 最近 64 个区块的差异保存在内存中，提供快速访问
- 状态查询从链表顶部向下搜索，未命中则查询 RocksDB
- 新区块到达时：推入新差异到头部，超过深度限制时将旧差异持久化到 RocksDB

### 状态更新

leafage-evm 支持两种状态更新方式：

- **Kafka + S3（主要方式）**：通过 Kafka 接收区块变更通知，从 S3 获取区块信息和状态差异
- **HTTP（备用方式）**：轮询修改版 Geth 的 `trace_debankBlock` RPC 接口

## 项目结构

```
leafage-evm/
├── bin/leafage-evm/           # CLI 入口
├── crates/
│   ├── leafage-evm-types/     # 类型定义
│   ├── leafage-evm-storage/   # 存储层（RocksDB/MDBX、StateTree）
│   ├── leafage-evm-rpc/       # JSON-RPC 实现
│   └── leafage-evm-chains/    # 链特定逻辑（BSC/Cosmos 预编译合约）
└── docs/                      # 文档
```

## License

MIT OR Apache-2.0
