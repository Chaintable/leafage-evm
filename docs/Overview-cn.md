# Leafage

可扩展、轻量、模块化的 EVM 状态查询基础设施。

## 概述

Leafage 是一套分布式架构，**将 EVM 状态查询与区块同步解耦**。传统方案中，共识、执行、P2P 网络、状态存储和 RPC 服务全部耦合在一个全节点进程中。Leafage 将这条流水线拆开：一个全节点负责同步和执行区块并导出状态变更，状态变更通过 Kafka + S3 分发给任意数量的轻量查询节点。

最终效果：可水平扩展的 EVM 状态查询，单节点仅需 ~90GB 存储（而非 1.3TB+），分钟级冷启动，区块同步和 RPC 查询零干扰。

### 什么是 Leafage？

Leafage 由五个组件组成，共同构成完整的 EVM 状态查询栈：

| 组件 | 语言 | 职责 |
|------|------|------|
| **go-ethereum-x** | Go | Geth 分叉，在区块执行过程中导出状态变更 |
| **pipeline** | Go | 序列化执行数据并通过 Kafka + S3 分发 |
| **consistency_checker** | Go | 校验副本间的区块一致性，确认后向外部消费者推送通知 |
| **leafage-evm** | Rust | 轻量 EVM 执行器，消费状态变更并提供 RPC 查询 |
| **nodex-proxy** | Go | JSON-RPC 网关，提供服务发现、负载均衡和智能路由 |

项目采用 MIT OR Apache-2.0 双许可证。

### 设计目标

**水平扩展。** 扩容查询能力应该只需启动新的 leafage-evm 实例——无需全链重新同步，无需 1TB+ 的磁盘，无需数小时等待。

**资源隔离。** 区块同步（CPU 密集的 EVM 执行 + 磁盘密集的状态写入）和 RPC 查询不应争抢资源。Leafage 将它们运行在不同进程中，需要时可以分布到不同机器。

**存储效率。** 查询节点只需要账户状态（balance、nonce、code、storage）。交易、收据和日志对 `eth_call` 无关，可以丢弃——存储从 1.3TB+ 降至 90GB。

**多链统一架构。** 同一套架构、部署工具和监控体系覆盖 Ethereum、Optimism、BSC、Cosmos EVM 和 Mantle。通过 `--evm-type` 参数切换链。

**快速恢复。** 故障节点应在分钟内可替换（S3 快照 + Kafka 追赶），而非数小时（P2P 重新同步）。

### 适用场景

**DeFi 协议和钱包**——需要跨多条链高吞吐执行 `eth_call`、`eth_estimateGas` 和批量合约调用。

**数据平台**——每天执行数百万次状态查询，需要独立于执行层扩展查询容量。

**基础设施团队**——管理多链部署，希望用统一、可观测的架构替代为每条链维护独立全节点。

如果你的瓶颈是 EVM 状态查询吞吐量，并且厌倦了通过增加全节点来扩容，Leafage 适合你。

## 架构

```
                          ┌───────────┐          ┌─────────────────────┐
                          │   客户端   │          │    外部消费者        │
                          └─────┬─────┘          └──────────┬──────────┘
                                │ JSON-RPC                  │ 订阅
                                ▼                           ▼
                       ┌─────────────────┐      ┌──────────────────────────┐
                       │   nodex-proxy   │      │   consistency_checker    │
                       │   (RPC 网关)     │      │   (一致性校验 & 分叉协调)  │
                       └────────┬────────┘      └──────────────────────────┘
                  etcd 服务发现  │  加权路由              ▲ 轮询 eth_blockNumber
                                │                       │
                  ┌─────────────┼─────────────┐          │
                  ▼             ▼             ▼          │
           ┌────────────┐┌────────────┐┌────────────┐    │
           │leafage-evm ││leafage-evm ││leafage-evm │────┘
           │(State 节点) ││(Archive)   ││(State 节点) │
           └─────┬──────┘└─────┬──────┘└─────┬──────┘
                 └─────────────┼─────────────┘
                               │ 消费 Kafka + S3
                               ▼
                 ┌──────────────────────────┐
                 │        pipeline          │
                 │   (Kafka + S3 数据管道)    │
                 └────────────┬─────────────┘
                              │ EVM 追踪钩子
                              ▼
                 ┌──────────────────────────┐
                 │     go-ethereum-x        │
                 │  (Geth 分叉 + Tracer)     │
                 └────────────┬─────────────┘
                              ▲ P2P 同步
                           以太坊网络
```

### 数据流：从区块到查询

一个新区块在变得可查询之前，经过五个阶段。与此同时，consistency_checker 独立验证副本收敛并通知外部消费者。

```
┌─ 1. 执行 ─────────────────────────────────────────────────────────────────┐
│  go-ethereum-x 通过 P2P 网络接收新块并执行。                                │
│  Pipeline Tracer 钩子在执行过程中触发，捕获状态变更、调用追踪和事件日志。       │
└──────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─ 2. 分发 ─────────────────────────────────────────────────────────────────┐
│  Pipeline 序列化数据并上传：                                                │
│  • Header + StateDiff → S3（内部桶）                                       │
│  • BlockFile（txs/traces/events）→ S3（外部桶）                             │
│  • BlockChangeNotification → Kafka                                        │
└──────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─ 3. 状态摄入 ──────────────────────────────────────────────────────────────┐
│  leafage-evm 直接消费 Kafka 通知，从 S3 拉取 Header 和 StateDiff，          │
│  将差异应用到内存中的 StateTree。                                           │
└──────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─ 4. 终结化 ───────────────────────────────────────────────────────────────┐
│  当区块深度超过 64 时，其状态从内存差异树持久化到 RocksDB。                    │
│  内存层随即释放。                                                          │
└──────────────────────────────────────────────────────────────────────────┘
                                     │
                                     ▼
┌─ 5. 查询服务 ──────────────────────────────────────────────────────────────┐
│  客户端 → nodex-proxy → leafage-evm。                                     │
│  代理根据请求的区块上下文路由到正确的节点类型（State/Archive）。                │
│  leafage-evm 在本地状态上执行 eth_call 并返回结果。                          │
└──────────────────────────────────────────────────────────────────────────┘

┌─ 并行：一致性校验 ────────────────────────────────────────────────────────┐
│  consistency_checker 同样消费 Kafka 通知。它轮询所有 leafage-evm 副本的       │
│  eth_blockNumber——仅当 ≥80% 的副本已同步到新高度后，才向外部 Kafka topic      │
│  发布确认通知。外部消费者（而非 leafage-evm）订阅该 topic，获得一致性保障的     │
│  区块更新。                                                                │
└──────────────────────────────────────────────────────────────────────────┘
```

## 核心组件

### go-ethereum-x

基于 go-ethereum v1.16.8 的分叉，新增两项关键能力，使其成为 Leafage 流水线的数据源：

**Pipeline Live Tracer。** 一个 `tracing.Hooks` 实现，在 EVM 执行过程中触发。实时捕获 balance/nonce/storage/code 变更、调用追踪和事件日志，然后交给 Pipeline 上传。启用方式：

```bash
geth --vmtrace pipeline --vmtrace.jsonconfig '{"kafka_brokers":"...", "s3_bucket":"..."}'
```

**`trace_debankBlock` RPC。** 通过单个 RPC 调用返回一个区块的完整执行输出（header + state diff + traces + events）。当 Kafka 不可用时作为 HTTP 回退模式使用——leafage-evm 可以直接轮询该端点。

**自定义追踪钩子**，添加到 go-ethereum 的 `core/tracing`：

| 钩子 | 触发时机 | 用途 |
|------|---------|------|
| `OnCommit` | 状态提交后 | 导出状态差异（修改的账户、存储、代码） |
| `OnGenesisBlock` | 创世块处理时 | 为初始分配生成合成交易和追踪 |
| `OnBlockDBStart` | 区块执行前 | 向追踪器提供 StateDB 访问 |

### Pipeline

嵌入 go-ethereum-x 运行的 Go 库。负责序列化执行数据并分发给下游消费者。

**双桶 S3 策略：**

| 桶 | 数据 | 格式 | 消费者 |
|----|------|------|--------|
| 内部桶（NodeX） | Header、StateDiff | JSON+gzip、RLP | leafage-evm |
| 外部桶（ChainTable） | BlockFile、Validation | JSON+gzip | 业务应用 |

**高可用**——通过 etcd Leader 选举实现。多个 Geth 实例可以运行 Pipeline，但仅 Leader 向 Kafka 发布消息，所有实例均上传 S3。

**两种集成模式：**

- **Live Tracer** — 实时接入 EVM 执行。零延迟，需要修改执行客户端。
- **RPC Tracer** — 按需调用 `trace_debankBlock`。无需修改核心代码，兼容任何 Geth 分叉。

### Consistency Checker

独立的校验层，与主数据管道并行运行。它**不在** Pipeline 和 leafage-evm 之间的数据路径上——而是观察两者，为外部消费者提供一致性保障的通知流。

**工作流程：**

1. **消费。** 订阅与 leafage-evm 相同的 Kafka topic（Pipeline 的区块变更通知）。
2. **轮询。** 对每个新块，通过 `eth_blockNumber` RPC 轮询所有 leafage-evm 副本。等待直到 ≥80%（可配置）的副本已同步到目标高度。
3. **校验。** 列出 S3 中同一高度的所有区块，检查分叉。如果存在多个不同的哈希，将非规范块标记为分叉。
4. **发布。** 仅在副本收敛之后，才向外部 Kafka topic 发布确认的 `OuterBlockChangeNotification`。

外部消费者（分析平台、索引器、业务应用——而非 leafage-evm）订阅该外部 topic。它们获得的保证是：每条收到的通知都已被 leafage-evm 集群应用并验证。

**Topic 对齐** — 在多版本部署中，Leader（通过 etcd 分布式锁选举）负责确保版本特定的 topic 和全局 topic 保持同步，通过快进或回退到共同祖先。

**协调栈：** etcd（Leader 选举 + 节点注册）、Kafka（消息传输）、S3（区块验证存储）、PebbleDB（本地双索引区块查询）。

### leafage-evm

基于 revm 和 alloy 构建的 Rust EVM 执行器。不参与 P2P 共识——直接消费 Pipeline 的 Kafka topic 获取区块通知，从 S3 拉取状态变更，提供 RPC 查询服务。

**状态管理：**

```
最近 64 个区块（内存）                                  终结化状态（磁盘）

Block N ──► Block N-1 ──► ... ──► Block N-63 ──►  CacheDiskLayer ──► RocksDB
(DiffLayer)  (DiffLayer)          (DiffLayer)       (读缓存)
```

- **内存差异树。** 最近 64 个区块以 `DiffLayer` 节点链表形式存储。每层仅保存相对于父层的状态变更（diff）。查询时从最新层向旧层遍历，直到找到目标键。
- **分叉支持。** 分叉区块存在于 `hash_diff_map` 中，但不在 `num_diff_map`（仅追踪规范链）中。通过区块哈希查询可访问分叉状态。
- **磁盘持久化。** 当区块深度超过 64 时，其累积状态被刷写到 RocksDB/MDBX。

**两种节点类型：**

| 类型 | 存储（ETH 主网） | 状态范围 | 适用场景 |
|------|----------------|---------|---------|
| **State 节点** | ~90 GB | 仅最新状态 | 大多数 RPC 查询 |
| **Archive 节点** | ~360 GB | 全部历史 | 对任意历史块执行 `eth_call` |

Archive 节点采用双写策略：`address || block_num` 用于历史查询（通过 RocksDB `seek_for_prev`），`address || u64::MAX` 作为最新状态的快速路径。

**状态更新模式：**

- **Kafka + S3**（生产环境）— 从 Kafka 消费 `BlockChangeNotification`，并行从 S3 拉取 Header 和 StateDiff，应用到 StateTree。
- **HTTP 轮询**（开发/回退）— 轮询 go-ethereum-x 的 `trace_debankBlock` RPC，通过回退到共同祖先处理链重组。

**服务注册。** 启动时，leafage-evm 在 etcd 的 `{chain_id}/nodes/{ip}_{port}` 路径注册自身，附带元数据（节点类型、状态类型）。nodex-proxy 自动发现。

### nodex-proxy

基于 Cloudwego Hertz 构建的 JSON-RPC 网关。将 leafage-evm 集群抽象为单一 RPC 端点。

**服务发现。** Watch etcd 中的节点注册。当 leafage-evm 实例启动或停止时，代理实时更新节点池。新节点通过 `getLatestBlock` RPC 进行健康检查后才被加入。

**智能路由。** 检查每个 RPC 请求中的区块参数：

- Latest / pending / 最近 64 个区块内 → **State 节点**
- 超过 64 个区块 → **Archive 节点**
- Cosmos 预编译合约调用 → **Native 节点**

**负载均衡。** 两种策略，可按链配置：

- **Random Weighted** — 基于节点权重的概率选择
- **Round-Robin Weighted** — 权重调节频率的确定性轮转

**自动故障转移：**

- `StateBlockNotFound` (-39006) → 重试 Archive 节点
- `CosmosPrecompile` (-39008) → 重试 Native 节点

**附加能力：**

- 方法级 RPS 限流
- 请求镜像到影子后端（异步，用于流量分析或灰度测试）
- 通过 HTTP 管理 API 动态调整权重
- 方法级路由规则（按节点配置包含/排除列表）

## 支持的链

leafage-evm 通过 `--evm-type` 参数支持多条 EVM 兼容链：

| 链 | 参数 | 特性 |
|----|------|------|
| **Ethereum** | `mainnet` | 标准 EVM，EIP-2935 blockhashes 合约 |
| **Optimism** | `op` | L2 Gas 计算，OVM 预编译合约，pre-bedrock RPC 转发 |
| **BSC** | `bsc` | Parlia 验证者黑名单，Tendermint/IAVL 预编译合约 |
| **Cosmos EVM** | `cosmos` | bech32 地址，p256 签名验证，原生代币处理 |
| **Mantle v2** | `mantlev2` | 基于 OP Stack |

添加新链需要实现 `EvmExecutor` trait——链特定的预编译合约、硬分叉规则和 Gas 计算封装在 `leafage-evm-chains` crate 中。

## RPC 接口

### 标准以太坊

| 方法 | 说明 |
|------|------|
| `eth_call` | 执行消息调用 |
| `eth_estimateGas` | 估算交易 Gas |
| `eth_getBalance` | 账户余额 |
| `eth_getCode` | 合约字节码 |
| `eth_getStorageAt` | 存储槽值 |
| `eth_getTransactionCount` | 账户 nonce |
| `eth_blockNumber` | 最新区块号 |
| `eth_getBlockByNumber` / `eth_getBlockByHash` | 仅返回区块头（无交易体） |
| `eth_chainId` | 链 ID |
| `eth_baseFee` | 当前 base fee |

### 扩展接口

| 方法 | 说明 |
|------|------|
| `eth_multiCall` | 并行批量执行多个调用，支持 `fast_fail` 和缓存控制 |
| `contractMultiCall` | 批量合约调用，支持 `BlockOverrides` |
| `simulateTransactions` | 模拟交易序列并预测结果 |
| `estimateGas` | 增强的 Gas 估算 |
| `pre_traceCall` | 单个调用追踪 |
| `pre_traceMany` | 批量调用追踪 |
| `getLatestBlock` / `getBlockByHeight` / `getBlockById` | 区块信息查询 |
| `blockIsValid` | 检查区块是否在规范链上 |

## 为什么选择 Leafage 而非单体 Geth？

### 水平扩展

| | 单体 Geth | Leafage |
|---|---|---|
| 扩容方式 | 每个实例独立同步全链 | 增加 leafage-evm 实例 |
| 单实例成本 | 1.3TB+ 磁盘 + 数小时同步 | 90GB 磁盘 + 分钟级 S3 快照 |
| 扩展上限 | 受 P2P 网络和磁盘 I/O 制约 | Kafka + S3 吞吐量（几乎无上限） |

Geth 扩容 10 倍查询能力意味着 10 个全节点，每个带有 1.3TB 冗余数据。Leafage 则是 10 个轻量查询节点，共享同一条数据管道。

### 资源隔离

单体 Geth 中，区块同步和 RPC 查询共享 CPU、内存和磁盘 I/O。高查询负载拖慢同步；同步高峰增加查询延迟。

Leafage 将二者分离：
- **go-ethereum-x** — 专注同步和执行
- **leafage-evm** — 专注查询

它们运行在不同进程、不同机器上，拥有独立的资源预算。

### 存储效率——Archive 小一个数量级

| | Geth Full | Geth Archive（flat state） | Geth Archive（+ trie） | leafage-evm State | leafage-evm Archive |
|---|---|---|---|---|---|
| ETH 主网 | ~900GB | **~2TB** | **~6.5TB** | **~90GB** | **~360GB** |
| 存储内容 | 区块、交易、收据、最新状态 | + 全量 flat state 历史 | + 历史 trie 数据 | 仅状态 | 状态 + 历史 |

对比在 Archive 节点上最为悬殊。Geth Archive 保留全量 flat state 历史需要 **~2TB**（如果同时保留历史 trie 数据则达 ~6.5TB）。leafage-evm Archive 仅存储账户状态历史（balance、nonce、code、storage），体积 **~360GB**——根据 Geth 配置不同，小了约 **5 到 18 倍**。

这不是压缩技巧，而是根本性的架构差异。leafage-evm 丢弃所有对 `eth_call` 无关的数据：交易体、收据、调用追踪、事件日志和 trie 节点。剩下的是在任意历史块上执行状态查询所需的最小数据集。

### 极致性能——RocksDB + revm

leafage-evm 为原始查询吞吐量而构建。性能优势来自存储引擎和执行运行时的组合：

**RocksDB + 专用列族。** 状态以扁平 key-value 布局存储——无需 Merkle Patricia Trie 遍历。账户查找是直接的 `db.get()` 操作，O(1) 复杂度。Archive 历史查询使用 RocksDB 的 `seek_for_prev`，配合按列族调优的 prefix extractor（账户 32 字节前缀，存储 64 字节前缀），即使面对数十亿键也能保持快速的迭代器定位。

**revm——最快的 EVM 实现。** leafage-evm 使用 revm（Rust EVM），与 Reth、Foundry 等性能关键的以太坊工具使用同一执行引擎。结合 Rust 的零成本抽象和 alloy 优化的类型系统，`eth_call` 执行避免了 Geth 携带的 Go GC 暂停和运行时开销。

**无 Trie 开销。** Geth 的每次状态访问都要经过 Merkle Patricia Trie——每个账户或存储槽需要多次 LevelDB 查找。leafage-evm 完全绕过了这一层：状态扁平存储，近期区块的内存差异树意味着大多数查询根本不需要触碰磁盘。

**内存热路径。** 最近 64 个区块存活在内存差异树中。对于绝大多数 RPC 查询（目标是 `latest` 或接近最新的区块），状态解析是纯内存遍历——无磁盘 I/O，无反序列化开销。

### 多链统一运维

没有 Leafage 时，每条链需要自己的全节点实现（Geth、op-node、bsc-node 等），部署、监控和升级流程各不相同。

有了 Leafage：
- **数据源：** go-ethereum-x + pipeline（每条链一套）
- **查询节点：** leafage-evm 配合 `--evm-type=mainnet|op|bsc|cosmos|mantlev2`
- **网关：** nodex-proxy 按 `chainId` 路由
- **监控：** 相同的 Prometheus 指标，相同的 Grafana 面板，所有链

一套架构。一套运维手册。覆盖所有链。

### 冷启动与恢复

| | 单体 Geth | Leafage |
|---|---|---|
| 冷启动 | 从 genesis 同步或导入 snapshot（数小时至数天） | 从 S3 下载 RocksDB 快照（分钟级），从 Kafka offset 追赶 |
| 故障恢复 | 重新同步或从备份恢复 | 新实例拉取快照 + 追赶 Kafka，自动注册 etcd |
| 扩容响应 | 慢（需要全量同步） | 快（快照 + 增量） |

### 增强的查询能力

leafage-evm 在标准 `eth_*` 接口之外，提供面向业务优化的 RPC 方法：

- **`contractMultiCall`** — 单次请求批量执行多个合约调用，支持覆盖区块参数
- **`simulateTransactions`** — 模拟交易序列并预测执行结果
- **`eth_multiCall`** — 并行批量调用，支持 `fast_fail` 和缓存控制
- **`pre_traceCall` / `pre_traceMany`** — 调用追踪，无需全节点 debug API 的开销

这些方法在标准 Geth 中要么不存在，要么需要额外的中间件实现。

### 总结

| 维度 | 单体 Geth | Leafage |
|------|-----------|---------|
| 扩展模型 | 垂直（加配置） | 水平（加实例） |
| Archive 存储 | ~2TB – 6.5TB（ETH 主网） | ~360GB（小 5–18 倍） |
| State 存储 | ~900GB | ~90GB |
| 状态访问 | MPT 遍历（多次磁盘读取） | 扁平 KV 查找，O(1) |
| EVM 运行时 | Go（GC 暂停，运行时开销） | revm / Rust（零成本抽象） |
| 热路径查询 | 始终触碰磁盘（LevelDB） | 内存差异树覆盖最近 64 块 |
| 查询与同步 | 耦合，互相干扰 | 隔离，可独立扩展 |
| 多链运维 | 每链一套全节点 | 统一架构，配置切换 |
| 冷启动 | 小时至数天 | 分钟级 |
| 故障恢复 | 慢，依赖 P2P | 快，基于 S3 快照 |
| 自定义 RPC | 修改 Geth 源码（Go） | Rust 原生实现，独立迭代 |

## 开始使用

- **[架构设计](./Architecture.md)** — leafage-evm 的 crate 结构和模块设计
- **[状态管理](./StateManage.md)** — 内存差异树、分叉处理和磁盘持久化
- **[状态更新](./StateUpdater.md)** — Kafka + S3 和 HTTP 更新模式
- **[数据库](./Database.md)** — RocksDB 列族、State 与 Archive 存储布局
- **[数据规范](./DataSpec.md)** — BlockStorageDiff、S3 对象和 Kafka 消息的线上格式
- **[部署指南](./deploy/Deploy.md)** — 包含 Beacon、Geth 和 leafage-evm 的 Docker Compose 部署
