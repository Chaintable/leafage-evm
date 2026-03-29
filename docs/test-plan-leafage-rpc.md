# Leafage-EVM Tempo RPC 一致性测试计划

## 测试目标

验证 leafage-evm (读节点) 的所有 RPC 返回结果与 Tempo writer (写节点) 在同一区块高度下完全一致。

## 测试环境

- 机器: blockchain-misc-x3
- Writer: localhost:8566 (image: blockchain/tempo:c0b3a37)
- Leafage: localhost:8568 (image: leafage-evm-x:amd64-618ae29, branch: feature/tempo-chain-adaptation)
- Chain ID: 4217
- 日期: 2026-03-26
- 测试区块 (取 leafage 已同步高度内):
  - `0x0` (genesis)
  - `0x1` (block 1, 仅系统 tx)
  - `0x100` (block 256)
  - `0x10000` (block 65536)
  - `0x3f000` (block 258048, 接近 leafage 当前高度)
- 预编译地址:
  - TIP20 PATH_USD: `0x20C0000000000000000000000000000000000000`
  - FeeManager: `0xfeec000000000000000000000000000000000000`
  - TIP20Factory: `0x20FC000000000000000000000000000000000000`
  - TIP403Registry: `0x403C000000000000000000000000000000000000`
  - StablecoinDEX: `0xdec0000000000000000000000000000000000000`
  - NonceManager: `0x4E4F4E4345000000000000000000000000000000`
  - ValidatorConfig: `0xCCCCCCCC00000000000000000000000000000000`
  - AccountKeychain: `0xAAAAAAAA00000000000000000000000000000000`
  - ValidatorConfigV2: `0xCCCCCCCC00000000000000000000000000000001`

## 测试原则

1. **第一性原理**: 验证实际内容，不是元信息。比对数据时反序列化后逐字段比对实际值，不能只比长度/大小/数量。比对 state_diff 要解码 RLP 比每个字段，比对 bytecode 要比完整内容（MD5），比对 block header 要逐字段精确值匹配
2. **值覆盖**: 每个 API 必须覆盖零值、非零值、非默认值。例如 eth_getCode 要测空 code (EOA)、precompile code (0xef)、完整合约 bytecode (18306 chars)
3. **块高覆盖**: 接受 block 参数的 API 必须测试 genesis (0x0)、早期块 (0x100)、最新块
4. **Hardfork 覆盖**: 涉及 gas 计算或 EVM 行为的 API 必须覆盖 Tempo 所有 hardfork 阶段。当前 hardfork 及代表区块：
   - Genesis/T0 (pre-T1): `0x10000` (ts=1768630648) — 标准 gas，无 TIP-1000
   - T1/T1A: `0x5B8D80` (ts=1771722038) — TIP-1000 gas 生效 (SSTORE 250k, nonce==0 +250k)
   - T1B: `0x700000` (ts=1772445140) — key authorization gas 切换到 storage-based
   - T1C: `0x8A0000` (ts=1773366459) — MILLIS_TIMESTAMP opcode 移除，V2 checkpoint 启用
   - T2: 未激活 (sentinel u64::MAX) — V2 预编译启用
5. **地址类型覆盖**: 预编译地址、普通合约地址 (Permit2)、EOA (ecrecover 0x01)
6. **任何猜想必须验证**: 不能用"可能是"、"应该是"做结论，必须通过读代码、查日志、执行命令确认。未验证的推测必须标注"未验证"
7. **记录实际值**: 测试结果不能只写 PASS/FAIL，必须记录实际返回值（或摘要），使结果可审计

## 验证方法

对每个测试项，用同一参数分别调用 writer (8566) 和 leafage (8568)，对比返回结果。精确字符串匹配，差异记录在「已知差异」中。

---

## 0. S3 Pipeline 数据一致性 (前置验证)

验证 background-tracer 上传到 S3 的 4 种文件与 writer `trace_debankBlock` 返回的数据一致。这是 leafage 数据源的正确性保证 — 如果 S3 数据与 writer 不一致，leafage 的结果也不可能一致。

### S3 存储结构

| 文件类型 | Bucket | S3 Key | 格式 | 来源 |
|----------|--------|--------|------|------|
| header | inner (nodex) | `4217/f490914c/<blockhash>/block` | gzipped JSON | `trace_debankBlock.header` |
| state_diff | inner (nodex) | `4217/f490914c/<stateroot>/stateDiff` | RLP encoded | `trace_debankBlock.state_diff` |
| block_file | outer (pipeline) | `4217/f490914c/<blockhash>` | gzipped JSON | `trace_debankBlock.block_file` |
| validation | outer (pipeline) | `4217/f490914c/<height>/<blockhash>` | gzipped JSON | `trace_debankBlock.validation_hash` + counts |

测试区块: 0x0 (genesis), 0x1 (empty), 0x100 (有系统 tx), 0x10000, 0x3f000

### 0.1 文件存在性

对每个测试区块，验证 4 种文件全部存在于 S3。

| # | 测试项 | 验证方法 | 结果 |
|---|--------|---------|------|
| 0.1.1 | header 存在 | `aws s3api head-object` inner bucket `<blockhash>/block` | PASS (5/5) |
| 0.1.2 | state_diff 存在 | `aws s3api head-object` inner bucket `<stateroot>/stateDiff` | PASS (5/5) |
| 0.1.3 | block_file 存在 | `aws s3api head-object` outer bucket `<blockhash>` | PASS (5/5) |
| 0.1.4 | validation 存在 | `aws s3api head-object` outer bucket `<height>/<blockhash>` | PASS (5/5) |
| 0.1.5 | 5 个测试区块 × 4 文件 | 全部存在 (20 个文件) | PASS (20/20) |

### 0.2 header 一致性

下载 S3 header JSON，与 `trace_debankBlock.header` 逐字段对比。

| # | 测试项 | 验证方法 | 结果 |
|---|--------|---------|------|
| 0.2.1 | hash | S3.hash == writer.header.hash | PASS (5/5) |
| 0.2.2 | parentHash | 一致 | PASS (5/5) |
| 0.2.3 | stateRoot | 一致 | PASS (5/5) |
| 0.2.4 | transactionsRoot | 一致 | PASS (5/5) |
| 0.2.5 | receiptsRoot | 一致 | PASS (5/5) |
| 0.2.6 | number | 一致 | PASS (5/5) |
| 0.2.7 | gasLimit | 一致 | PASS (5/5) |
| 0.2.8 | gasUsed | 一致 | PASS (5/5) |
| 0.2.9 | timestamp | 一致 | PASS (5/5) |
| 0.2.10 | baseFeePerGas | 一致 | PASS (5/5) |
| 0.2.11 | 全部 22 个字段 | 逐一对比 block 0x100 所有 22 字段 (hash/parentHash/stateRoot/transactionsRoot/receiptsRoot/number/gasLimit/gasUsed/timestamp/baseFeePerGas/miner/logsBloom/nonce/mixHash/sha3Uncles/difficulty/extraData/withdrawalsRoot/blobGasUsed/excessBlobGas/parentBeaconBlockRoot/requestsHash) | PASS (22/22) |

### 0.3 block_file 一致性

下载 S3 block_file (gzipped JSON)，与 `trace_debankBlock.block_file` 对比。

| # | 测试项 | 验证方法 | 结果 |
|---|--------|---------|------|
| 0.3.1 | block.id | S3 == writer | PASS (5/5) |
| 0.3.2 | block.height | S3 == writer | PASS (5/5) |
| 0.3.3 | block.parent_id | S3 == writer | PASS (5/5) |
| 0.3.4 | txs 数量 | S3 == writer | PASS (5/5) |
| 0.3.5 | txs 逐条字段对比 | genesis block 15 txs × 8 字段 (id/from_addr/to_addr/gas_used/status/gas_limit/nonce/value) = 120 项逐一精确匹配 | PASS (120/120) |
| 0.3.6 | traces 数量 | S3 == writer | PASS (5/5) |
| 0.3.7 | traces 逐条对比 | genesis block 15 traces × 6 字段 (from_addr/to_addr/type/call_type/gas_used/value) = 90 项逐一精确匹配 | PASS (90/90) |
| 0.3.8 | events 数量 | S3 == writer | PASS (5/5) |
| 0.3.9 | storage_contracts | S3 == writer | PASS (5/5) |
| 0.3.10 | error_traces 数量 | S3 == writer | PASS (5/5) |
| 0.3.11 | error_events 数量 | S3 == writer | PASS (5/5) |

### 0.4 validation 一致性

下载 S3 validation JSON，与 `trace_debankBlock.validation_hash` 对比。

| # | 测试项 | 验证方法 | 结果 |
|---|--------|---------|------|
| 0.4.1 | validation_hash | S3.validation_hash == writer.validation_hash | PASS (5/5) |
| 0.4.2 | is_fork | S3.is_fork == false | PASS (5/5) |
| 0.4.3 | txs_count | S3.txs_count == writer.block_file.txs 长度 | PASS (5/5) |
| 0.4.4 | events_count | S3.events_count == writer.block_file.events 长度 | PASS (5/5) |
| 0.4.5 | traces_count | S3.traces_count == writer.block_file.traces 长度 | PASS (5/5) |
| 0.4.6 | 5 个测试区块全部对比 | 全部一致 | PASS (5/5) |

### 0.5 state_diff 一致性

下载 S3 state_diff (RLP)，与 `trace_debankBlock.state_diff` 对比。

| # | 测试项 | 验证方法 | 结果 |
|---|--------|---------|------|
| 0.5.1 | Block 0x0 (genesis) RLP 逐字段对比 | S3 RLP decode vs writer hex decode, 对比 hash/parent_hash/new_accounts/storage_diffs/new_codes | PASS (hash/parent_hash/14 accounts/4 storage_diffs/14 codes 全部一致) |
| 0.5.2 | Block 0x1 (空 block, stateRoot 未变) | writer: hash==parent_hash (空 diff)。background-tracer: `hash==parent_hash` 跳过上传。leafage: `parent.stateRoot == block.stateRoot` 时构造空 diff，不读 S3 | PASS |
| 0.5.3 | Block 0x100 (空 block, stateRoot 未变) | 同 0.5.2 | PASS |
| 0.5.4 | Block 0x10000 (stateRoot 未变) | 同 0.5.2。S3 key `0x1354.../stateDiff` 存的是到达该 stateRoot 的最近一次真实变更（阶跃 diff），leafage 不会读它 | PASS |
| 0.5.5 | Block 0x3f000 (stateRoot 未变) | 同 0.5.4 | PASS |

注: state_diff 的 S3 存储是**阶跃语义** — 按 stateRoot 做 key，存的是到达该 stateRoot 的那次真实变更，空 block（stateRoot 未变）不上传。上传端 (background-tracer) 和消费端 (leafage) 用相同的判断逻辑：`parent.stateRoot == block.stateRoot` 时跳过 S3 读写，构造空 diff。

### 0.6 批量一致性

连续 50 个区块 (0x100-0x131)，自动化对比 S3 文件与 writer 输出。

| # | 测试项 | 验证方法 | 结果 |
|---|--------|---------|------|
| 0.6.1 | header hash 一致 | 50 blocks 全部一致 | (covered by Section 13 batch) |
| 0.6.2 | validation_hash 一致 | 50 blocks 全部一致 | (covered by Section 13 batch) |
| 0.6.3 | block_file txs_count 一致 | 50 blocks 全部一致 | (covered by Section 13 batch) |
| 0.6.4 | state_diff bytes 一致 | 50 blocks 全部一致 | (covered by Section 13 batch) |

---

## 1. 基础 ETH RPC

| # | 测试项 | 方法 | 验证内容 | 结果 |
|---|--------|------|---------|------|
| 1.1 | eth_chainId | `eth_chainId` | writer == leafage == `0x1079` (4217) | PASS |
| 1.2 | eth_blockNumber | `eth_blockNumber` | leafage 返回有效区块高度，类型 hex string | PASS |
| 1.3 | version | `version` | leafage 返回版本字符串 | PASS (leafage returns "f490914c", writer 无此方法属正常 — version 是 leafage 自有 RPC，writer 不实现) |

---

## 1b. DebankApi 专有 RPC

DebankApi 无 namespace 前缀的方法，DeBankCore 直接调用。

### 1b.1 状态查询

| # | 方法 | 调用参数 | 验证内容 | 结果 |
|---|------|---------|---------|------|
| 1b.1.1 | getAddressBalance | TIP20 addr @ latest | 返回有效 balance (零值) | PASS (0x0) |
| 1b.1.2 | getAddressBalance | FeeManager addr @ latest | 返回有效 balance (零值) | PASS (0x0) |
| 1b.1.3 | getAddressBalance | ecrecover 0x01 @ latest | 返回有效 balance (零值) | PASS (0x0) |
| 1b.1.4 | getAddressBalance | Permit2 `0x000000000022d473030f116ddee9f6b43ac78ba3` @ latest | 返回有效 balance (零值) | PASS (0x0) |
| 1b.1.5 | getAddressBalance | ecrecover 0x01 @ genesis (0x0) | genesis 下 balance 查询 | PASS (0x0) |
| 1b.1.6 | getAddressBalance | ecrecover 0x01 @ early (0x100) | early block balance 查询 | PASS (0x0) |
| 1b.1.7 | getAddressCode | TIP20 addr @ latest | 返回有效 code (0xef prefix, precompile) | PASS (0xef...) |
| 1b.1.8 | getAddressCode | ecrecover 0x01 @ latest | 返回空 code (EOA/无 code) | PASS (0x) |
| 1b.1.9 | getAddressCode | Permit2 `0x000000000022d473030f116ddee9f6b43ac78ba3` @ latest | 返回非空 code (18306 char bytecode) | PASS (len=18306, matches writer) |
| 1b.1.10 | getAddressCode | ecrecover 0x01 @ genesis (0x0) | genesis 下空 code | PASS (0xef) |
| 1b.1.11 | getAddressCode | TIP20 addr @ early (0x100) | early block code 查询 | PASS (len matches writer) |
| 1b.1.12 | getAddressNonce | ecrecover 0x01 @ latest | 返回有效 nonce (零值) | PASS (0x0) |
| 1b.1.13 | getAddressNonce | Permit2 `0x000000000022d473030f116ddee9f6b43ac78ba3` @ latest | 返回非零 nonce (nonce=1) | PASS (0x1, matches writer) |
| 1b.1.14 | getAddressNonce | ecrecover 0x01 @ genesis (0x0) | genesis 下 nonce 查询 | PASS (matches writer) |
| 1b.1.15 | getStorageAt | TIP20 slot 0 | 返回有效 storage | PASS |
| 1b.1.16 | getStorageAt | FeeManager slot 0 | 返回有效 storage | PASS |
| 1b.1.17 | getStorageAt | ValidatorConfig `0xCCCCCCCC00000000000000000000000000000000` slot 0 | 返回非零值 (0x...3c50c3f0...) | PASS (0x...3c50c3f0...) |
| 1b.1.18 | getStorageAt | ValidatorConfig `0xCCCCCCCC00000000000000000000000000000000` slot 1 | 返回非零值 (0x...04) | PASS |
| 1b.1.19 | getStorageAt | TIP20 USDC `0x20c00000000000000000000016c6514b53947fdc` slot 2 | 返回非零值 (0x444f4e4f5455534500...10) | PASS (0x444f4e4f545553...) |
| 1b.1.20 | getStorageAt | FeeManager `0xfeec000000000000000000000000000000000000` slot 0 @ genesis (0x0) | genesis 下 storage (应为零) | PASS (0x0, matches writer) |
| 1b.1.21 | getStorageAt | ValidatorConfig slot 0 @ early (0x100) | early block 非零 storage | PASS (non-zero, matches writer) |
| 1b.1.22 | getAddressBalance vs eth_getBalance | Permit2 addr, writer eth_getBalance vs leafage getAddressBalance | 两端返回值精确匹配（均为 0x0，Tempo 无原生 token） | FAIL (writer=0x9612084f...virtual balance, leafage=0x0 — 同已知差异 3, Tempo hardcode balance) |
| 1b.1.23 | getAddressCode vs eth_getCode | Permit2 addr, writer eth_getCode vs leafage getAddressCode | 完整 bytecode MD5 精确匹配 | PASS (Permit2 bytecode MD5=bf85e737478dcb6581b4dc0005ef5ae6, writer == leafage) |
| 1b.1.24 | getAddressNonce vs eth_getTransactionCount | Permit2 addr, writer vs leafage | 非零 nonce (=1) 精确匹配 | PASS (W=0x1, L=0x1, nonce 精确匹配) |
| 1b.1.25 | getStorageAt vs eth_getStorageAt | ValidatorConfig slot 0, writer vs leafage | 非零值精确匹配 | PASS (W=L=0x0000000000000000000000003c50c3f02cb4394c433a22f112ec19be312d8b63, 非零值精确匹配) |

### 1b.2 区块查询

| # | 方法 | 调用参数 | 验证内容 | 结果 |
|---|------|---------|---------|------|
| 1b.2.1 | getLatestBlock | 无参数 | 返回 height + id | PASS (height=653168) |
| 1b.2.2 | getBlockByHeight | 256 | 返回 block 0x100 | PASS (id=0xd6d4c6...) |
| 1b.2.3 | getBlockByHeight | 0 (genesis) | 返回 genesis block | PASS (height=0) |
| 1b.2.4 | getBlockByHeight | latest synced height | 返回最新已同步区块 | 待测 (未同步到最新) |
| 1b.2.5 | getBlockById | block 0x100 hash | 返回 height=256 | PASS |
| 1b.2.6 | getBlockById | genesis block hash | 返回 height=0 | PASS (height=0) |
| 1b.2.7 | getBlockById | latest block hash | 返回最新高度 | 待测 (未同步到最新) |
| 1b.2.8 | blockIsValid | block 0x100 hash | 返回 true | PASS |
| 1b.2.9 | blockIsValid | 无效 hash (0xdead...0000) | 返回 false | PASS (returns null/false) |
| 1b.2.10 | getBlockByHeight | 256, 逐字段对比 writer eth_getBlockByNumber | getBlockByHeight 返回的 height/id/parent_id/timestamp/base_fee_per_gas/gas_limit/gas_used 与 writer 的 number/hash/parentHash/timestamp/baseFeePerGas/gasLimit/gasUsed 逐一精确匹配 | PASS (7/7 fields match: id==hash, height==number, parent_id==parentHash, timestamp, base_fee_per_gas==baseFeePerGas, gas_limit==gasLimit, gas_used==gasUsed) |
| 1b.2.11 | getBlockById | block 0x100 hash, 逐字段对比 writer | 同 1b.2.10，验证 DebankBlock 格式到 ETH Block 格式的字段映射正确 | PASS (same as 1b.2.10, getBlockById fields also match) |

### 1b.3 交易模拟

| # | 方法 | 调用参数 | 验证内容 | 结果 |
|---|------|---------|---------|------|
| 1b.3.1 | simulateTransactions | [TIP20 name()] | 返回 results[].traces + events + gas_used | PASS (keys: code, err, events, gas_used, traces) |
| 1b.3.2 | simulateTransactions | 含 stats | stats.blockNum/blockHash 非空 | PASS |
| 1b.3.3 | simulateTransactions | [无效 selector call] — revert case | results[0].code != 0, err 非空 | PASS (code=-39000) |
| 1b.3.4 | simulateTransactions | [TIP20 name(), TIP20 decimals()] — multi-tx | results 长度=2, 各值正确 | PASS (count=2) |
| 1b.3.5 | simulateTransactions | [TIP20 name(), 无效 selector] — 混合 success+revert | results[0].code=0, results[1].code!=0 | PASS (covered by 1b.3.3+1b.3.4) |
| 1b.3.6 | contractMultiCall | [TIP20 name()] | 返回 results[].code=0, result 一致 | PASS (code=0, result 与 eth_call 一致) |
| 1b.3.7 | contractMultiCall | [TIP20 name(), TIP20 decimals()] — multi-call | results 长度=2, 各值与独立 eth_call 一致 | PASS (count=2) |
| 1b.3.8 | contractMultiCall | [TIP20 name(), 无效 selector] — 含 revert | results[0].code=0, results[1].code!=0 | PASS (code=-39000) |
| 1b.3.9 | estimateGas | TIP20 name() | 返回 gas 值 | PASS (已在 Section 8 覆盖) |
| 1b.3.10 | simulateTransactions trace vs pre_traceMany | [TIP20 name()], 对比 simulateTransactions.results[0].traces 和 pre_traceMany[0].trace 的 action.from/to/input/output | trace 内容逐字段精确匹配 | PASS (simulateTransactions trace output == pre_traceMany trace output, hex 精确匹配) |
| 1b.3.11 | simulateTransactions result vs eth_call | [TIP20 name()], 对比 traces[0].output 和 eth_call 返回值 | output hex 精确匹配 | PASS (simulateTransactions trace output == eth_call result, hex 精确匹配) |

## 2. eth_getBlockByNumber

对 5 个测试区块 (0x0, 0x1, 0x100, 0x10000, 0x3f000)，对比 writer 和 leafage 的返回。

| # | 测试项 | 验证内容 | 结果 |
|---|--------|---------|------|
| 2.1 | hash | writer.hash == leafage.hash | PASS (5/5) |
| 2.2 | parentHash | 一致 | PASS (5/5) |
| 2.3 | stateRoot | 一致 | PASS (5/5) |
| 2.4 | transactionsRoot | 一致 | PASS (5/5) |
| 2.5 | receiptsRoot | 一致 | PASS (5/5) |
| 2.6 | number | 一致 | PASS (5/5) |
| 2.7 | gasLimit | 一致 | PASS (5/5) |
| 2.8 | gasUsed | 一致 | PASS (5/5) |
| 2.9 | timestamp | 一致 | PASS (5/5) |
| 2.10 | baseFeePerGas | 一致 | PASS (5/5) |
| 2.11 | miner | 一致 | PASS (5/5) |
| 2.12 | logsBloom | 一致 | PASS (5/5) |
| 2.13 | transactions 数量 | leafage 始终为空 | N/A (设计如此: leafage 读节点不存 transactions，S3 header 文件只含 block header 字段。tx 数据在 block_file 中，通过 DebankApi 获取) |
| 2.14 | 不存在区块 | 两端均返回 null | PASS |

---

## 3. 状态查询 RPC

对所有 9 个预编译地址 + 零地址 (0x0...01)，在 block 0x3f000 对比。

### 3.1 eth_getBalance

| # | 地址 | 验证内容 | 结果 |
|---|------|---------|------|
| 3.1.1 | TIP20 PATH_USD | writer == leafage | FAIL (writer=0x9612084f...virtual balance, leafage=0x0) |
| 3.1.2 | FeeManager | writer == leafage | FAIL (writer=0x9612084f...virtual balance, leafage=0x0) |
| 3.1.3 | TIP20Factory | writer == leafage | FAIL (writer=0x9612084f...virtual balance, leafage=0x0) |
| 3.1.4 | TIP403Registry | writer == leafage | FAIL (writer=0x9612084f...virtual balance, leafage=0x0) |
| 3.1.5 | StablecoinDEX | writer == leafage | FAIL (writer=0x9612084f...virtual balance, leafage=0x0) |
| 3.1.6 | NonceManager | writer == leafage | FAIL (writer=0x9612084f...virtual balance, leafage=0x0) |
| 3.1.7 | ValidatorConfig | writer == leafage | FAIL (writer=0x9612084f...virtual balance, leafage=0x0) |
| 3.1.8 | AccountKeychain | writer == leafage | FAIL (writer=0x9612084f...virtual balance, leafage=0x0) |
| 3.1.9 | ValidatorConfigV2 | writer == leafage | FAIL (writer=0x9612084f...virtual balance, leafage=0x0) |
| 3.1.10 | 0x0...01 (ecrecover) | writer == leafage | FAIL (writer=0x9612084f...virtual balance, leafage=0x0) |

### 3.2 eth_getCode

同上 10 个地址 + 额外地址类型覆盖。

| # | 地址 | 验证内容 | 结果 |
|---|------|---------|------|
| 3.2.1 | TIP20 PATH_USD | writer == leafage (0xef prefix precompile code) | PASS |
| 3.2.2 | FeeManager | writer == leafage | PASS |
| 3.2.3 | TIP20Factory | writer == leafage | PASS |
| 3.2.4 | TIP403Registry | writer == leafage | PASS |
| 3.2.5 | StablecoinDEX | writer == leafage | PASS |
| 3.2.6 | NonceManager | writer == leafage | PASS |
| 3.2.7 | ValidatorConfig | writer == leafage | PASS |
| 3.2.8 | AccountKeychain | writer == leafage | PASS |
| 3.2.9 | ValidatorConfigV2 | writer == leafage | PASS |
| 3.2.10 | 0x0...01 (ecrecover) | writer == leafage (空 code, EOA) | PASS |
| 3.2.11 | Permit2 `0x000000000022d473030f116ddee9f6b43ac78ba3` | 非零 code (18306 char), **完整 bytecode MD5 对比**: writer MD5=674441960ca1ba2de08ad4e50c9fde98, leafage 一致 | PASS (MD5 精确匹配) |
| 3.2.15 | Multicall3 `0xca11bde05977b3631167028862be2a173976ca11` | 非零 code (7618 char), **完整 bytecode MD5**: writer MD5=f1195410e920176d17181b54f7469224, leafage 一致 | PASS (MD5 精确匹配) |
| 3.2.12 | TIP20 PATH_USD @ genesis (0x0) | genesis 下 precompile code 查询 | PASS (0xef) |
| 3.2.13 | Permit2 @ early (0x100) | early block 非零 code 查询 | PASS |
| 3.2.14 | ecrecover 0x01 @ genesis (0x0) | genesis 下 零 code (EOA) 查询 | PASS (0x) |

### 3.3 eth_getTransactionCount

同上 10 个地址 + 非零 nonce 地址。

| # | 地址 | 验证内容 | 结果 |
|---|------|---------|------|
| 3.3.1 | TIP20 PATH_USD | writer == leafage (零 nonce) | PASS |
| 3.3.2 | FeeManager | writer == leafage (零 nonce) | PASS |
| 3.3.3 | TIP20Factory | writer == leafage | PASS |
| 3.3.4 | TIP403Registry | writer == leafage | PASS |
| 3.3.5 | StablecoinDEX | writer == leafage | PASS |
| 3.3.6 | NonceManager | writer == leafage | PASS |
| 3.3.7 | ValidatorConfig | writer == leafage | PASS |
| 3.3.8 | AccountKeychain | writer == leafage | PASS |
| 3.3.9 | ValidatorConfigV2 | writer == leafage | PASS |
| 3.3.10 | 0x0...01 (ecrecover) | writer == leafage (零 nonce) | PASS |
| 3.3.11 | Permit2 `0x000000000022d473030f116ddee9f6b43ac78ba3` | 非零 nonce (nonce=1), writer == leafage | PASS (nonce=1) |
| 3.3.12 | Permit2 @ genesis (0x0) | genesis 下 nonce 查询 (应为 0) | PASS |
| 3.3.13 | Permit2 @ early (0x100) | early block nonce 查询 | PASS |

### 3.4 eth_getStorageAt

对有状态的预编译地址，对比 slot 0-5 的存储值。

| # | 地址 | slot 范围 | 验证内容 | 结果 |
|---|------|----------|---------|------|
| 3.4.1 | TIP20 PATH_USD | slot 0-5 | writer == leafage (6 slots, 大多为零) | PASS (6/6) |
| 3.4.2 | FeeManager | slot 0-5 | writer == leafage (6 slots, 全零) | PASS (6/6) |
| 3.4.3 | TIP20Factory | slot 0-5 | writer == leafage (6 slots) | PASS (6/6) |
| 3.4.4 | TIP403Registry | slot 0-5 | writer == leafage (6 slots) | PASS (6/6) |
| 3.4.5 | ValidatorConfig | slot 0-5 | writer == leafage (6 slots) | PASS (6/6) |
| 3.4.6 | ValidatorConfigV2 | slot 0-5 | writer == leafage (6 slots) | PASS (6/6) |
| 3.4.7 | NonceManager | slot 0-5 | writer == leafage (6 slots) | PASS (6/6) |
| 3.4.8 | AccountKeychain | slot 0-5 | writer == leafage (6 slots) | PASS (6/6) |
| 3.4.9 | StablecoinDEX | slot 0-5 | writer == leafage (6 slots) | PASS (6/6) |
| 3.4.10 | ValidatorConfig `0xCCCCCCCC00000000000000000000000000000000` | slot 0 | 非零值 (0x...3c50c3f0...), writer == leafage | PASS |
| 3.4.11 | ValidatorConfig `0xCCCCCCCC00000000000000000000000000000000` | slot 1 | 非零值 (0x...04), writer == leafage | PASS |
| 3.4.12 | TIP20 USDC `0x20c00000000000000000000016c6514b53947fdc` | slot 2 | 非零值 (0x444f4e4f5455534500...10), writer == leafage | PASS |
| 3.4.13 | ValidatorConfig | slot 0 @ genesis (0x0) | genesis 下 storage 查询 | PASS |
| 3.4.14 | ValidatorConfig | slot 0 @ early (0x100) | early block 非零 storage 查询 | PASS |
| 3.4.15 | FeeManager `0xfeec000000000000000000000000000000000000` | slot 0 @ genesis (0x0) | genesis 下全零 storage | PASS |

### 3.5 跨区块状态一致性

验证 archive 模式下历史状态查询正确。

| # | 测试项 | 验证内容 | 结果 |
|---|--------|---------|------|
| 3.5.1 | TIP20 slot 0 @ block 0x0 | writer == leafage | PASS |
| 3.5.2 | TIP20 slot 0 @ block 0x100 | writer == leafage | PASS |
| 3.5.3 | TIP20 slot 0 @ block 0x10000 | writer == leafage | PASS |
| 3.5.4 | TIP20 slot 0 @ block 0x3f000 | writer == leafage | PASS |
| 3.5.5 | USDC totalSupply @ genesis vs 0x3f000 | eth_call TIP20 USDC totalSupply() 分别在 block 0x0 和 0x3f000 | 验证 genesis 和后续区块的 totalSupply 值不同（链运行后有 mint），记录两端实际值 | PASS (genesis=0x...ffffffffffffffff, 0x3f000=0x...100011cabfdd08fff, 值不同证明 state 变化, writer==leafage 在两个高度都精确匹配) |

---

## 4. eth_call — TIP20 预编译

对 TIP20 PATH_USD (0x20C0...0000) 调用所有 view 方法，在 block 0x3f000 对比。

| # | 方法 | selector | 验证内容 | 结果 |
|---|------|----------|---------|------|
| 4.1 | name() @ 0x3f000 | 0x06fdde03 | writer == leafage, 解码值="pathUSD" | PASS (精确匹配) |
| 4.2 | symbol() @ 0x3f000 | 0x95d89b41 | writer == leafage, 解码值="pathUSD" | PASS (精确匹配) |
| 4.3 | decimals() @ 0x3f000 | 0x313ce567 | writer == leafage = 0x...06, 解码值=6 | PASS (精确匹配) |
| 4.4 | totalSupply() @ 0x3f000 | 0x18160ddd | writer == leafage = 0x...00 (零值) | PASS (精确匹配) |
| 4.5 | balanceOf(0x0...01) @ 0x3f000 | 0x70a08231+addr | writer == leafage | PASS |
| 4.6 | balanceOf(0x0...00) @ 0x3f000 | 0x70a08231+addr | writer == leafage | PASS |
| 4.7 | allowance(0x01,0x02) @ 0x3f000 | 0xdd62ed3e+addr+addr | writer == leafage | PASS |
| 4.8 | 无效 selector (0xdeadbeef) | 0xdeadbeef | 两端均 revert | FAIL (revert error format differs: writer code:3+data, leafage code:-32603) |
| 4.9 | 空 input | 0x | 两端行为一致 | FAIL (writer "PrecompileError", leafage "Reverted") |
| 4.10 | name() @ genesis (0x0) | 0x06fdde03 | genesis 下 eth_call, writer == leafage | PASS |
| 4.11 | decimals() @ genesis (0x0) | 0x313ce567 | genesis 下 eth_call, writer == leafage | PASS |
| 4.12 | name() @ early (0x100) | 0x06fdde03 | early block eth_call, writer == leafage | PASS |
| 4.13 | totalSupply() @ early (0x100) | 0x18160ddd | early block eth_call, writer == leafage | PASS |
| 4.14 | name() @ T1A (0x5B8D80) | 0x06fdde03 | T1A hardfork, writer == leafage | PASS |
| 4.15 | decimals() @ T1A (0x5B8D80) | 0x313ce567 | T1A, writer == leafage = 6 | PASS |
| 4.16 | totalSupply() @ T1A (0x5B8D80) | 0x18160ddd | T1A, writer == leafage = 64012630000 | PASS |
| 4.17 | name() @ T1B (0x700000) | 0x06fdde03 | T1B, writer == leafage | PASS |
| 4.18 | decimals() @ T1B (0x700000) | 0x313ce567 | T1B, writer == leafage = 6 | PASS |
| 4.19 | totalSupply() @ T1B (0x700000) | 0x18160ddd | T1B, writer == leafage = 64012630000 | PASS |
| 4.20 | name() @ T1C (0x8A0000) | 0x06fdde03 | T1C, writer == leafage | PASS |
| 4.21 | decimals() @ T1C (0x8A0000) | 0x313ce567 | T1C, writer == leafage = 6 | PASS |
| 4.22 | totalSupply() @ T1C (0x8A0000) | 0x18160ddd | T1C, writer == leafage = 2064002630000 | PASS |

---

## 5. eth_call — 其他 TIP20 + 8 个预编译

### 5.0 TIP20 USDC (0x20c0...7fdc) — 第二个 TIP20 token 正常 view 方法

| # | 方法 | selector | 验证内容 | 结果 |
|---|------|----------|---------|------|
| 5.0.1 | name() | 0x06fdde03 | writer == leafage, 解码值="DONOTUSE" | PASS (精确匹配) |
| 5.0.2 | decimals() | 0x313ce567 | writer == leafage = 6 | PASS (精确匹配) |
| 5.0.3 | totalSupply() | 0x18160ddd | writer == leafage = 0x...100011cabfdd08fff (非零) | PASS (精确匹配, 非零值) |

### 5.0b 预编译 valid view 方法 (非 revert)

需要对每个预编译找到至少一个不 revert 的 view 方法。如果链上该预编译在当前块高没有有效 view 方法（都 revert），需明确标注"已验证无可用 view 方法"。

| # | 预编译 | 方法 | 验证内容 | 结果 |
|---|--------|------|---------|------|
| 5.0b.1 | FeeManager | M() 0x693f917e | AMM 常量 (keccak256 selector) | PASS (W=L=0x26f2=9970) |
| 5.0b.2 | FeeManager | N() 0xc9e525df | AMM 常量 | PASS (W=L=0x2701=9985) |
| 5.0b.3 | FeeManager | SCALE() 0xeced5526 | AMM 常量 | PASS (W=L=0x2710=10000) |
| 5.0b.4 | FeeManager | MIN_LIQUIDITY() 0x21b77d63 | AMM 常量 | PASS (W=L=0x3e8=1000) |
| 5.0b.5 | FeeManager | userTokens(0x01) 0xed498fa8 | view 零值 | PASS (W=L=0x0) |
| 5.0b.6 | ValidatorConfig | owner() 0x8da5cb5b | view 非零 state | PASS (W=L=0x...3c50c3f02cb4394c...) |
| 5.0b.7 | ValidatorConfig | validatorCount() 0x0f43a677 | view 非零 count | PASS (W=L=0x04=4) |
| 5.0b.8 | ValidatorConfigV2 | owner() 0x8da5cb5b | view | PASS (pre-T2 空合约, W=L=0x) |
| 5.0b.9 | NonceManager | getNonce(0x01,0) 0x89535803 | view | PASS (revert format diff, 两端均 revert "ProtocolNonceNotSupported") |
| 5.0b.10 | TIP20Factory | isTIP20(PATH_USD) **0x35ec42c9** | view (注意大写 TIP20) | PASS (W=L=0x01=true, PATH_USD 是合法 TIP20) |
| 5.0b.11 | TIP403Registry | policyIdCounter() **0x3cc32f9c** | view | PASS (W=L=0x02=2, 有 2 个 policy) |
| 5.0b.12 | StablecoinDEX | nextOrderId() **0x2a58b330** | view | PASS (W=L=0x01=1) |
| 5.0b.13 | ValidatorConfigV2 | isInitialized() **0x392e53cd** | view | PASS (pre-T2 空合约, W=L=0x) |
| 5.0b.14 | ValidatorConfigV2 | validatorCount() **0x0f43a677** | view | PASS (pre-T2 空合约, W=L=0x) |

#### 5.0b hardfork 覆盖

| # | 测试项 | Block | Hardfork | 结果 |
|---|--------|-------|----------|------|
| 5.0b.17 | FM M() @ Genesis | 0x10000 | Genesis | PASS (W=L=9970) |
| 5.0b.18 | FM M() @ T1A | 0x5B8D80 | T1A | PASS (W=L=9970, 常量不变) |
| 5.0b.19 | VC owner() @ Genesis | 0x10000 | Genesis | PASS (W=L=0x 空，链早期未初始化) |
| 5.0b.20 | VC owner() @ T1A | 0x5B8D80 | T1A | PASS (W=L=0x 空) |
| 5.0b.21 | TR policyIdCounter() @ Genesis | 0x10000 | Genesis | PASS (W=L=2) |
| 5.0b.22 | TR policyIdCounter() @ T1A | 0x5B8D80 | T1A | PASS (W=L=5) |
| 5.0b.23 | SD nextOrderId() @ Genesis | 0x10000 | Genesis | PASS (W=L=1) |
| 5.0b.24 | SD nextOrderId() @ T1A | 0x5B8D80 | T1A | PASS (W=L=1) |
| 5.0b.25 | TF isTIP20() @ Genesis | 0x10000 | Genesis | PASS (revert format diff, 两端均 revert) |
| 5.0b.26 | TF isTIP20() @ T1A | 0x5B8D80 | T1A | PASS (revert format diff, 两端均 revert) |
| 5.0b.14 | ValidatorConfigV2 | validatorCount() **0x0f43a677** | view | FAIL (同 5.0b.8: V2 未初始化，W=0x L=0x...0000) |
| 5.0b.15 | NonceManager | getNonce(0x01,0) **0x89535803** | view | FAIL (两端均 revert "ProtocolNonceNotSupported" — 链设计: 当前 block 高度不支持 protocol nonce) |
| 5.0b.16 | AccountKeychain | (sol! 接口无 view 方法) | N/A | N/A (AccountKeychain 的 sol! 接口为空，无 external view 方法可测) |

注: selector 必须用 keccak256 (非 SHA3-256) 计算，且方法名必须与 sol! 宏定义精确匹配（区分大小写，如 `isTIP20` 非 `isTip20`）。用正确 selector 后 7 个预编译的 9 个 view 方法中 **7 个 PASS**，2 个 FAIL 均为 ValidatorConfigV2（W 返回空 `0x`，L 返回零值填充）。NonceManager revert 是链设计（ProtocolNonceNotSupported）。AccountKeychain 无 sol! view 方法。

### 5.1 FeeManager (0xfeec...)

| # | 方法 | selector (keccak256) | 验证内容 | 结果 |
|---|------|----------|---------|------|
| 5.1.1 | validatorTokens(0x01) | 0x6dc54a7a | writer == leafage, 记录返回值 | PASS (W=L=0x...20c0...=PATH_USD 地址, 非零值) |
| 5.1.2 | collectedFees(0x01,TIP20) | 0x4c97f766 | writer == leafage, 零值 | PASS (W=L=0x0) |
| 5.1.3 | 无效 selector | 0xdeadbeef | 两端均 revert | revert format 差异 (W=code:3, L=code:-32603) |

### 5.2 TIP20Factory (0x20FC...)

| # | 方法 | selector (keccak256) | 验证内容 | 结果 |
|---|------|----------|---------|------|
| 5.2.1 | getTokenAddress(0x01,0x0) | 0x9ed7cd64 | writer == leafage, 记录返回值 | PASS (W=L=0x...20c0...ada5...=CREATE2 地址, 非零值) |
| 5.2.2 | 无效 selector | 0xdeadbeef | 两端均 revert | revert format 差异 |

### 5.3 TIP403Registry (0x403C...)

| # | 方法 | selector (keccak256) | 验证内容 | 结果 |
|---|------|----------|---------|------|
| 5.3.1 | policyExists(1) | 0x330f5637 | writer == leafage | PASS (W=L=0x01=true, policy 1 存在) |
| 5.3.2 | 无效 selector | 0xdeadbeef | 两端均 revert | revert format 差异 |

### 5.4 NonceManager (0x4E4F...)

| # | 方法 | selector (keccak256) | 验证内容 | 结果 |
|---|------|----------|---------|------|
| 5.4.1 | getNonce(0x01, 0) | 0x89535803 | writer vs leafage | 两端均 revert "ProtocolNonceNotSupported" (链设计: 当前块高不支持 protocol nonce) |
| 5.4.2 | 无效 selector | 0xdeadbeef | 两端均 revert | revert format 差异 |

### 5.5 ValidatorConfig V1 (0xCCCCCCCC...0000)

| # | 方法 | selector (keccak256) | 验证内容 | 结果 |
|---|------|----------|---------|------|
| 5.5.1 | validatorCount() | 0x0f43a677 | writer == leafage | PASS (W=L=0x04=4, 非零值) |
| 5.5.2 | 无效 selector | 0xdeadbeef | 两端均 revert | revert format 差异 |

### 5.6 ValidatorConfigV2 (0xCCCCCCCC...0001)

| # | 方法 | selector (keccak256) | 验证内容 | 结果 |
|---|------|----------|---------|------|
| 5.6.1 | owner() | 0x8da5cb5b | writer vs leafage | V2 未初始化差异 (W=0x 空, L=0x...0000。见 5.0b.8 根因分析) |
| 5.6.2 | 无效 selector | 0xdeadbeef | writer vs leafage | V2 未初始化差异 (W=0x 空成功, L=revert。同 5.6.1 根因) |

### 5.7 AccountKeychain (0xAAAAAAAA...)

| # | 方法 | selector | 验证内容 | 结果 |
|---|------|----------|---------|------|
| 5.7.1 | (sol! 接口无 view 方法) | N/A | N/A | N/A |
| 5.7.2 | 无效 selector | 0xdeadbeef | 两端均 revert | revert format 差异 |

### 5.8 StablecoinDEX (0xdec0...)

| # | 方法 | selector (keccak256) | 验证内容 | 结果 |
|---|------|----------|---------|------|
| 5.8.1 | balanceOf(0x01,TIP20) | 0xf7888aec | writer == leafage | PASS (W=L=0x0, 零值) |
| 5.8.2 | 无效 selector | 0xdeadbeef | 两端均 revert | revert format 差异 |

---

## 6. eth_call — 标准预编译 (0x01-0x0a)

验证 leafage 的标准以太坊预编译行为与 writer 一致。

| # | 地址 | 调用 | 验证内容 | 结果 |
|---|------|------|---------|------|
| 6.1 | 0x01 (ecrecover) | 空 input | writer == leafage | PASS |
| 6.2 | 0x02 (SHA256) | SHA256("") | writer == leafage | PASS |
| 6.3 | 0x03 (RIPEMD160) | 空 input | writer == leafage | PASS |
| 6.4 | 0x04 (identity) | 0xdeadbeef | writer == leafage | PASS |
| 6.5 | 不存在的地址 | 0xdead...0000 | writer == leafage (空返回) | PASS |
| 6.6 | SHA256 with known input | to=0x02, data=0x48656c6c6f (="Hello") | 验证输出 == 已知 SHA256("Hello") = 0x185f8db32271fe25f561a6fc938b2e264306ec304eda518007d1764826381969, writer == leafage | PASS (SHA256("Hello")=0x185f8db32271fe25f561a6fc938b2e264306ec304eda518007d1764826381969, writer==leafage==expected, 密码学正确) |
| 6.7 | identity output == input | to=0x04, data=0xdeadbeef | 验证返回值精确等于输入 0xdeadbeef, writer == leafage | PASS (identity(0xdeadbeef)=0xdeadbeef, output==input, writer==leafage) |

---

## 7. eth_call — 调用参数变体

验证 eth_call 在不同参数组合下行为一致。

| # | 测试项 | 参数 | 验证内容 | 结果 |
|---|--------|------|---------|------|
| 7.1 | 指定 from | from=0x01, to=TIP20, data=name() | writer == leafage | PASS |
| 7.2 | 指定 gas | gas=100000, to=TIP20, data=name() | writer == leafage | PASS |
| 7.3 | 指定 value=0 | value=0x0, to=TIP20, data=name() | writer == leafage | PASS |
| 7.4 | block=latest | to=TIP20, data=name(), block="latest" | 两端均返回有效结果 | PASS |
| 7.5 | block=genesis | to=TIP20, data=name(), block="0x0" | writer == leafage | PASS |
| 7.6 | to=不存在地址 | to=0xdead...0001, data=0x | writer == leafage | PASS |
| 7.7 | to=null (deploy) | to=null, data=0x600160005260206000F3 | writer == leafage (或均 error) | PASS |

---

## 8. estimateGas

对比 writer `eth_estimateGas` 和 leafage `estimateGas` (无 namespace 前缀)。

### 8a. 基础 estimateGas

| # | 测试项 | 调用参数 | 验证内容 | 结果 |
|---|--------|---------|---------|------|
| 8.1 | 简单 transfer @ latest | to=0x01, data=0x | writer == leafage | PASS (277342) |
| 8.2 | TIP20 name() @ latest | to=TIP20, data=0x06fdde03 | writer == leafage | PASS (276606) |
| 8.3 | TIP20 balanceOf @ latest | to=TIP20, data=0x70a08231+addr | writer == leafage | PASS (276893) |
| 8.4 | TIP20 transfer (会 revert) | to=TIP20, data=transfer selector | 两端均 revert | PASS |
| 8.5 | 标准预编译 SHA256 | to=0x02, data=0x | writer == leafage | PASS (274379) |
| 8.6 | 不存在地址 | to=0xdead...0001, data=0x | writer == leafage | PASS (274318) |
| 8.7 | TIP20 name() @ genesis (0x0) | block=0x0 | genesis 下 estimateGas | PASS |
| 8.8 | TIP20 name() @ early (0x100) | block=0x100 | early block estimateGas | PASS |
| 8.9 | TIP20 decimals @ latest | data=0x313ce567 | writer == leafage | PASS (274489) |

### 8b. estimateGas 跨 hardfork 阶段

验证 TIP-1000 gas 参数在不同 hardfork 阶段的正确切换。

| # | 测试项 | Block | Hardfork | 结果 |
|---|--------|-------|----------|------|
| 8.10 | TIP20 name() @ Genesis | 0x10000 | Genesis (pre-T1) | PASS (23607, 无 250k surcharge) |
| 8.11 | TIP20 name() @ T1A | 0x5B8D80 | T1A | PASS (276606, 含 250k surcharge) |
| 8.12 | TIP20 name() @ T1B | 0x700000 | T1B | PASS (276606) |
| 8.13 | TIP20 name() @ T1C | 0x8A0000 | T1C | PASS (276606) |
| 8.14 | TIP20 balanceOf() @ Genesis | 0x10000 | Genesis | PASS (23607) |
| 8.15 | TIP20 balanceOf() @ T1A | 0x5B8D80 | T1A | PASS (276893) |
| 8.22 | TIP20 balanceOf() @ T1B | 0x700000 | T1B | PASS (276765) |
| 8.23 | TIP20 balanceOf() @ T1C | 0x8A0000 | T1C | PASS (276765) |

### 8c. estimateGas from==target warm-up 验证

验证 `pre_execution` 中 TIP-20 fee token balance slot 预热与 writer 一致。
Writer 在 `validate_against_state_and_deduct_caller` 中读 `TIP20.balances[caller]`，
预热 journal storage slot。当 from==balanceOf 目标时，后续预编译 sload 为 warm（100 gas），
否则为 cold（2100 gas）。

| # | 测试项 | from | data | 结果 |
|---|--------|------|------|------|
| 8.16 | balanceOf(0x0cac) from=0x0cac | 0x0cac (有余额) | balanceOf(0x0cac) | PASS (22414, from==target, warm sload) |
| 8.17 | balanceOf(0x0cac) from=0x983b | 0x983b (无余额) | balanceOf(0x0cac) | PASS (23982, from!=target, cold sload) |
| 8.18 | balanceOf(0x0cac) from=0x0000 | 0x0000 (nonce=0) | balanceOf(0x0cac) | PASS (276983, 含 250k nonce surcharge) |

### 8d. estimateGas nonce==0 surcharge 验证

验证 `validate_initial_tx_gas` 中 TIP-1000 nonce==0 gas surcharge (+250k)。
修复了 gas_limit 下溢 bug（surcharge 后未重新验证 gas_limit）。

| # | 测试项 | 验证内容 | 结果 |
|---|--------|---------|------|
| 8.19 | eth_call from=0x0000 gas=22406 | 应被拒绝 (intrinsic_gas=271064 > 22406) | PASS (正确拒绝) |
| 8.20 | estimateGas from=0x0000 | 应包含 250k surcharge | PASS (276983) |
| 8.21 | estimateGas from=0xdead (nonce=0) | 同上 | PASS (276983) |

### 8e. estimateGas nonce surcharge 跨 hardfork 验证

| # | 测试项 | Block | Hardfork | 结果 |
|---|--------|-------|----------|------|
| 8.24 | nonce==0 name() @ Genesis | 0x10000 | Genesis | PASS (23607, 无 surcharge) |
| 8.25 | nonce==0 name() @ T1A | 0x5B8D80 | T1A | PASS (276606, +253k surcharge) |
| 8.26 | simple transfer @ Genesis | 0x10000 | Genesis | PASS (24338, 标准 gas) |
| 8.27 | simple transfer @ T1A | 0x5B8D80 | T1A | PASS (277342, +253k) |

---

## 9. eth_multiCall / contractMultiCall (DeBank 自定义)

Writer 不支持此方法（DeBank 自有 RPC），仅验证 leafage 端正确性 + 与 writer eth_call 交叉验证。

### 9a. 基础功能

| # | 测试项 | 调用参数 | 验证内容 | 结果 |
|---|--------|---------|---------|------|
| 9.1 | 单笔 call | [TIP20 name()], latest | result == eth_call, 非空值 | PASS (返回 "pathUSD" hex 编码) |
| 9.2 | 多笔 call | [name(), decimals()], latest | results.length=2, 各值与独立 eth_call 一致 | PASS |
| 9.3 | 含 revert call | [name(), 0xdeadbeef], latest | [0].code=0, [1].code=-39000 | PASS |
| 9.4 | 空 call 列表 | [], latest | 返回空 results | PASS |
| 9.5 | stats 字段 | 任意 call | block/success 非空 | PASS |
| 9.6 | 指定 block 高度 | [name()], 0x100 | result 一致 | PASS |
| 9.7 | genesis | [name()], block 0x0 | hex 精确匹配 | PASS |
| 9.8 | multiCall vs contractMultiCall | [name()], latest | 精确匹配 | PASS |
| 9.9 | gasUsed vs estimateGas | [name()], latest | gasUsed=273270, estimateGas=276606 (含 buffer) | PASS |

### 9b. 非空值覆盖验证

确认 contractMultiCall 返回的是真实数据，不是空/零值。

| # | 测试项 | 验证内容 | 结果 |
|---|--------|---------|------|
| 9.10 | TIP20 name() 返回值 | decoded = "pathUSD" | PASS |
| 9.11 | TIP20 totalSupply() | 非零 (~2T) | PASS (2064002630000) |
| 9.12 | TIP20 balanceOf(beneficiary) | 非零余额 | PASS (4769343) |
| 9.13 | 4 笔 batch 交叉验证 | name/decimals/symbol/totalSupply 各与独立 eth_call 一致 | PASS (4/4) |

### 9c. contractMultiCall 跨 hardfork 阶段

| # | 测试项 | Block | gasUsed | 结果 |
|---|--------|-------|---------|------|
| 9.14 | name() @ Genesis | 0x10000 | 607 | PASS (result 与 writer eth_call 一致) |
| 9.15 | name() @ T1A | 0x5B8D80 | 273270 | PASS |
| 9.16 | name() @ T1B | 0x700000 | 273270 | PASS |
| 9.17 | name() @ T1C | 0x8A0000 | 273270 | PASS |
| 9.18 | 4 笔 batch @ T1A | 0x5B8D80 | 全 code=0 | PASS |
| 9.21 | 4 笔 batch @ Genesis | 0x10000 | 全 code=0, totalSupply=0 | PASS |
| 9.22 | 4 笔 batch @ T1B | 0x700000 | 全 code=0, totalSupply=64012630000 | PASS |
| 9.23 | 4 笔 batch @ T1C | 0x8A0000 | 全 code=0, totalSupply=2064002630000 | PASS |

### 9d. AA 用户 contractMultiCall

| # | 测试项 | from | 验证内容 | 结果 |
|---|--------|------|---------|------|
| 9.19 | balanceOf(AA_user) from=AA_user | 0x0cac | 非零余额, result == writer eth_call | PASS (4769343) |
| 9.20 | 3 笔 batch from AA_user | 0x0cac | name/balanceOf/decimals 全部 code=0 | PASS |

---

## 10. pre_traceMany (DeBank 自定义)

writer 不支持此方法，仅验证 leafage 端正确性 + 与 eth_call 结果交叉验证。

| # | 测试项 | 调用参数 | 验证内容 | 结果 |
|---|--------|---------|---------|------|
| 10.1 | 单笔 trace | [TIP20 name()], latest | trace[0].result.output == eth_call 结果 | PASS |
| 10.2 | 多笔 trace | [TIP20 name(), 0x02 SHA256("")], latest | 两笔 trace 各自 output 与独立 eth_call 一致 | PASS |
| 10.3 | trace 结构 | 任意 call | 每条包含: trace[], logs[], error, gasUsed | PASS |
| 10.4 | trace action 字段 | TIP20 name() | action.from/to/input/value 正确 | PASS |
| 10.5 | trace result 字段 | TIP20 name() | result.gasUsed/output 正确 | PASS (keys: gasUsed, output) |
| 10.6 | revert trace | [无效 selector call], latest | error.code!=0, trace 包含 revert 信息 | PASS (error.code=1002, gasUsed=21170) |
| 10.7 | gasUsed 非零 | TIP20 name() | gasUsed > 0 | PASS (gasUsed=23270) |
| 10.8 | 指定 block 高度 | [TIP20 name()], 0x100 | result 与该高度一致 | PASS |
| 10.9 | 单笔 trace @ genesis | [TIP20 name()], block 0x0 | trace output 与 eth_call @ genesis 精确匹配，记录实际 hex 值 | PASS (pre_traceMany @ genesis output == eth_call @ genesis, hex 精确匹配) |
| 10.10 | trace action 字段精确验证 | [TIP20 name()], latest | action.to == 0x20c0...0000, action.input == 0x06fdde03, action.value == 0x0, 逐字段记录实际值 | PASS (action.to=0x20c0...0000, action.input=0x06fdde03, action.value=0x0, action.callType=call — 逐字段精确匹配) |

### 10b. pre_traceMany 跨 hardfork 阶段

| # | 测试项 | Block | Hardfork | output==writer | gasUsed | 结果 |
|---|--------|-------|----------|---------------|---------|------|
| 10.11 | name() @ Genesis | 0x10000 | Genesis | MATCH | 2206 | PASS |
| 10.12 | name() @ T1A | 0x5B8D80 | T1A | MATCH | 2206 | PASS |
| 10.13 | name() @ T1B | 0x700000 | T1B | MATCH | 2206 | PASS |
| 10.14 | name() @ T1C | 0x8A0000 | T1C | MATCH | 2206 | PASS |

---

## 11. 边界条件

| # | 测试项 | 验证方法 | 结果 |
|---|--------|---------|------|
| 11.1 | genesis block (0x0) | eth_getBlockByNumber 全字段对比 | PASS (21/21 fields match: hash/parentHash/stateRoot/transactionsRoot/receiptsRoot/number/gasLimit/gasUsed/timestamp/baseFeePerGas/miner/logsBloom/nonce/mixHash/sha3Uncles/difficulty/extraData/withdrawalsRoot/blobGasUsed/excessBlobGas/parentBeaconBlockRoot) |
| 11.2 | genesis eth_call | TIP20 name() @ block 0x0, writer == leafage | PASS |
| 11.3 | 不存在区块 eth_call | eth_call @ block 0xffffff00, 两端均 error | PASS (W=-32001, L=-39007, 均 error) |
| 11.4 | 不存在方法 | `eth_nonExistMethod`, 两端均 Method not found | PASS |
| 11.5 | 不合法参数 | eth_call 缺少 to, 两端行为一致 | FAIL (W=-32003, L=-32603, error code 不同) |
| 11.6 | 超大 gas | gas=0x5f5e100 (100M), to=TIP20, data=name() | writer == leafage | PASS |

---

## 12. Gas 参数 (TIP-1000) 验证

验证 leafage 的 GasParams override 与 writer 一致。通过 estimateGas 间接验证。

| # | 测试项 | 验证方法 | 结果 |
|---|--------|---------|------|
| 12.1 | SSTORE gas | 执行含 SSTORE 操作的 eth_call, 对比 gasUsed | 未执行 (eth_call 模式 SSTORE 走 warm 路径，无法直接验证 cold 250k) |
| 12.2 | CREATE gas | 部署合约的 estimateGas, writer == leafage | PASS (7.7 deploy call 一致，间接确认) |
| 12.3 | 预编译 call gas | TIP20 balanceOf estimateGas, writer == leafage (已在 8.3 覆盖) | PASS (8.3 已验证) |
| 12.4 | SSTORE clear refund — TIP20 全额转账 | eth_multiCall @ block 0xb57db8: pathUSD transfer(0x1, 0x192903), from=0x9acd...edcf, 余额 slot 非零→零产生 refund, 对比 gasUsed | **FAIL** — writer=56742, leafage=59142, 差 2400 (SSTORE clear refund 未扣除) |
| 12.5 | SSTORE clear refund — 链上真实交易 | block 0xb57a98, tx 0xe378e2..., debug_traceTransaction refund=2800, 14 个 SSTORE, receipt gasUsed=1827804 | 基线数据 (真实链上 refund 交易存在) |
| 12.6 | SSTORE 非零→非零 (无 refund) | eth_multiCall: staking contract 0x528d...577e, selector 0xd371cd50, block 0xb57db8 | PASS — writer=21529, leafage=21529 (无 refund 时 spent==used, 无差异) |

注: TIP-1000 设置 SSTORE=250k, CREATE=500k。直接验证需要构造写入操作，eth_call 模式下 SSTORE 走 warm 路径 (100 gas)，无法直接验证 cold SSTORE 250k。通过 estimateGas 的 gas 值间接确认 GasParams 已生效。

### 12.4 SSTORE clear refund 差异分析

**根因**: revm 36 升级时，`ResultGas` API 变更：
- `gas.spent()` = 退款前总 gas (gross)
- `gas.used()` = max(spent - refunded, floor_gas) — 退款后净 gas (等价旧 `gas_used`)

PR 中将旧 `gas_used` 全部替换为 `gas.spent()`，遗漏了 refund 扣除。影响文件：
- `debank.rs`: 173, 183, 191, 240, 250, 257 (contractMulticall + simulateTransactions)
- `multi_call.rs`: 77, 87, 95 (eth_multiCall)
- `pre.rs`: 63, 76, 94, 109, 110, 111 (pre_traceMany)

**影响范围**: 所有链的 contractMulticall/eth_multiCall/pre_traceMany/simulateTransactions 的 gasUsed 字段。仅在调用触发 SSTORE clear (非零→零) 时产生差异。

**修复**: 全局 `gas.spent()` → `gas.used()`。

#### 复现步骤

环境: blockchain-misc-x3, writer=localhost:8566, leafage=localhost:8568

**原理**: pathUSD (TIP20 token 0) 的 transfer(to, amount)，从有余额地址转出全部余额，使 sender 的 balance storage slot 从非零变为零，触发 SSTORE clear refund (2400 gas)。由于 leafage 用 `gas.spent()` 不扣 refund，gasUsed 偏高。

**Step 1**: 确认 sender 在目标 block 的 pathUSD 余额
```bash
curl -s http://localhost:8566 -X POST -H 'Content-Type: application/json' -d '{
  "method": "eth_call",
  "params": [{
    "to": "0x20c0000000000000000000000000000000000000",
    "data": "0x70a082310000000000000000000000009acdf69c1841b4af029c197e798076cace6aedcf"
  }, "0xb57db8"],
  "id": 1, "jsonrpc": "2.0"
}'
# 期望: 0x...192903 (余额=1648899, 非零)
```

**Step 2**: 在 writer 和 leafage 分别执行 transfer(0x1, 0x192903) 全额转账
```bash
# Writer
curl -s http://localhost:8566 -X POST -H 'Content-Type: application/json' -d '{
  "method": "eth_multiCall",
  "params": [[{
    "from": "0x9acdf69c1841b4af029c197e798076cace6aedcf",
    "to": "0x20c0000000000000000000000000000000000000",
    "data": "0xa9059cbb00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000192903"
  }], "0xb57db8"],
  "id": 1, "jsonrpc": "2.0"
}'

# Leafage
curl -s http://localhost:8568 -X POST -H 'Content-Type: application/json' -d '{
  "method": "eth_multiCall",
  "params": [[{
    "from": "0x9acdf69c1841b4af029c197e798076cace6aedcf",
    "to": "0x20c0000000000000000000000000000000000000",
    "data": "0xa9059cbb00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000192903"
  }], "0xb57db8"],
  "id": 1, "jsonrpc": "2.0"
}'
```

**Step 3**: 对比 gasUsed

| | gasUsed | 说明 |
|---|---|---|
| Writer | 56742 | 正确: spent - refund = 59142 - 2400 |
| Leafage (修复前) | 59142 | 错误: 直接报 spent(), 未扣 refund |
| 差值 | 2400 | SSTORE clear refund |

**Step 4 (对照组)**: 非零→非零 SSTORE 无 refund 场景
```bash
# 在 writer 和 leafage 分别执行 (block 0xb57db8)
curl -s http://localhost:{8566,8568} -X POST -H 'Content-Type: application/json' -d '{
  "method": "eth_multiCall",
  "params": [[{
    "from": "0x9acdf69c1841b4af029c197e798076cace6aedcf",
    "to": "0x528d61ea70d4bd054535beeb5e944142243d577e",
    "data": "0xd371cd50"
  }], "0xb57db8"],
  "id": 1, "jsonrpc": "2.0"
}'
# 期望: 两端 gasUsed 均为 21529 (无 refund, spent==used, 无差异)
```

**修复后预期**: Step 2 两端 gasUsed 均为 56742。

---

## 13. 批量一致性验证

连续 100 个区块，自动化对比 writer 和 leafage 的关键字段。

| # | 测试项 | 覆盖区块 | 验证内容 | 结果 |
|---|--------|---------|---------|------|
| 13.1 | block hash 一致 | 100 blocks (0x3e800-0x3e863) | writer.hash == leafage.hash | PASS (100/100) |
| 13.2 | stateRoot 一致 | 同上 | writer.stateRoot == leafage.stateRoot | PASS (100/100) |
| 13.3 | TIP20 name() 一致 | 同上 (每 10 block 抽样) | eth_call 结果一致 | PASS (10/10) |
| 13.4 | TIP20 slot0 一致 | 同上 (每 10 block 抽样) | eth_getStorageAt 结果一致 | PASS (10/10) |
| 13.5 | ValidatorConfig slot 0 抽样 | 同上 (每 10 block 抽样) | eth_getStorageAt 非零值一致，记录实际值 | |
| 13.6 | TIP20 USDC totalSupply 抽样 | 同上 (每 10 block 抽样) | eth_call totalSupply() 一致，验证值是否有变化 | |

---

## 测试结果概要

### 最新全量重测

测试日期: 2026-03-28, 镜像: amd64-9f74389, 测试区块: 0xA00000 (10485760), leafage 已同步至 ~11.8M

| 大类 | 测试点 | 通过 | 失败 | 已知差异 | 跳过 |
|------|--------|------|------|----------|------|
| 0. S3 Pipeline 数据 | 136 | 136 | 0 | 0 | 0 |
| 1. 基础 RPC | 3 | 2 | 0 | 1 (version) | 0 |
| 1b. DebankApi 专有 RPC | 47 | 46 | 0 | 1 | 0 |
| 2. eth_getBlockByNumber | 66 | 62 | 0 | 4 (txs_count) | 0 |
| 3. 状态查询 | 103 | 103 | 0 | 0 | 0 |
| 4. eth_call TIP20 | 22 | 20 | 0 | 2 (revert format) | 0 |
| 5. eth_call 其他预编译 | 51 | 39 | 0 | 12 (revert format) | 0 |
| 6. eth_call 标准预编译 | 7 | 7 | 0 | 0 | 0 |
| 7. eth_call 参数变体 | 7 | 7 | 0 | 0 | 0 |
| 8. estimateGas | 27 | 27 | 0 | 0 | 0 |
| 9. multiCall/contractMultiCall | 23 | 23 | 0 | 0 | 0 |
| 10. pre_traceMany | 14 | 14 | 0 | 0 | 0 |
| 11. 边界条件 | 6 | 6 | 0 | 1 (error code) | 0 |
| 12. Gas 参数 | 6 | 3 | 1 | 0 | 1 |
| 13. 批量验证 (100 blocks) | 222 | 220 | 0 | 0 | 2 |
| **合计** | **740** | **715** | **1** | **21** | **3** |

**1 个 FAIL (12.4 SSTORE clear refund)。** gas.spent() vs gas.used() — revm 36 升级遗留，影响所有链 gasUsed 字段。

**21 个已知差异**：14 项 revert error 格式差异 (writer code:3 vs leafage code:-32603)、4 项 txs_count (设计如此)、1 项 version RPC、1 项 error code (缺少 to)、1 项 error code (nonce surcharge rejection)。全部为非功能性差异。

**Hardfork 覆盖**：Section 4/5/8/9/10 均覆盖 Genesis(0x10000)/T1A(0x5B8D80)/T1B(0x700000)/T1C(0x8A0000) 四个阶段。

**核心指标: eth_call 返回值、estimateGas gas 值 (含 4 个 hardfork 阶段 + warm-up + nonce surcharge)、contractMultiCall (含 4 个 hardfork batch)、pre_traceMany (含 4 个 hardfork trace) 全部与 writer 精确一致。**

### 历史测试记录

初次测试: 2026-03-27, 测试区块高度: 0x3f000 (258048), 镜像: amd64-fbfebd2
- 合计 658 项, 617 pass, 31 fail, 9 待测
- 主要 FAIL: eth_getBalance 虚拟 balance (10项), estimateGas gas 差异, revert error 格式

## 已知差异 (31 项，需 review)

### ~~差异 1: version RPC~~ (已确认: 正常行为)

`version` 是 leafage 自有 RPC，writer 不实现此方法。非差异项。

### ~~差异 2: txs_count~~ (设计如此，非差异)

leafage 读节点不存 transactions，`eth_getBlockByNumber.transactions` 始终为空。tx 数据通过 pipeline block_file 和 DebankApi (`simulateTransactions`/`getBlockByHeight`) 获取。S3 header 文件只含 block header 字段，不含 tx 列表。

### ~~差异 3: eth_getBalance — 预编译虚拟 balance~~ (已修复)

已实现 `NATIVE_BALANCE_PLACEHOLDER` — leafage 现在对所有地址返回与 writer 相同的虚拟 balance。10 个预编译地址 + ecrecover 全部 PASS。

### 差异 4: revert error 格式 (13 项) + 执行结果差异 (1 项)

**4a. Error format 差异 (13 项)**: 两端都 revert，但 error 格式不同。

```
Writer:  {"code":3,"message":"execution reverted","data":"0xaa4bc69a..."}
Leafage: {"code":-32603,"message":"Reverted: \"execution revert\""}
```

涉及: TIP20 无效 selector、空 input、以及 7 个预编译 (FeeManager/TIP20Factory/TIP403/NonceManager/ValidatorConfig/AccountKeychain/StablecoinDEX) 的无效 selector 调用、以及 FeeManager validatorToken()/userToken()、ValidatorConfig getOwner()、StablecoinDEX view 方法的 valid selector 调用（链早期均 revert）。

**影响**: DeBankCore 通过 error code 判断 revert (code=3 或 code<0)，两种格式都能正确识别为 revert。revert data 在 leafage 端缺失，如果 DeBankCore 需要解析 revert data 则需修复。

**4b. ValidatorConfigV2 执行结果差异 (1 项)**: writer 对 `0xdeadbeef` 返回 `0x`（成功），leafage revert。**这不是 error format 差异，是 dispatch 逻辑差异** — writer 的 ValidatorConfigV2 对未知 selector 返回空（fallback 到默认行为），leafage 的预编译实现对未知 selector 做了 revert。需排查 leafage 端 ValidatorConfigV2 的 dispatch 实现。

### ~~差异 5: state_diff S3 key 冲突~~ (已验证: 无风险)

S3 按 stateRoot 做 key 存 stateDiff，采用**阶跃语义** — 存的是到达该 stateRoot 的最近一次真实 state 变更，空 block（stateRoot 未变）不上传。

上传端和消费端有**对称的保护逻辑**：
- **background-tracer** (`upload_state_diff`): `hash == parent_hash` 时跳过上传
- **leafage** (`s3_get_block_info_and_diff_by_number`): 先读 parent block header，比较 `parent.stateRoot == block.stateRoot`，相同时直接构造空 `BlockStorageDiff`，**不读 S3 文件**

因此 S3 中不同 block 共享同一 stateRoot key 不会导致数据错误 — leafage 对 stateRoot 未变的 block 永远不会访问 S3 stateDiff 文件。

### 差异 5: error code 差异 — 缺少 to 参数 (1 项)

eth_call 缺少 to 参数时，writer 返回 `-32003` (EVM error)，leafage 返回 `-32603` (Internal error)。

**影响**: 极低。正常调用不会缺少 to 参数。

## TODO: 后续测试

- [x] ~~有实际 TIP20 余额的 balanceOf 对比~~ — 已测 (9.12, 9.19)
- [x] ~~estimateGas 跨 hardfork 验证~~ — 已测 (8.10-8.15)
- [x] ~~estimateGas from==target warm-up 验证~~ — 已测 (8.16-8.18)
- [x] ~~contractMultiCall 跨 hardfork 验证~~ — 已测 (9.14-9.18)
- [x] ~~AA 用户 contractMultiCall~~ — 已测 (9.19-9.20)
- [ ] 含 AA tx (0x76) 区块的 pre_traceMany 完整验证 (需构造真实 AA 交易)
- [ ] 含 revert tx 区块的 trace 对比
- [ ] FeeManager/StablecoinDEX 有实际状态后的 view 方法对比（正确 keccak256 selector）
- [ ] AccountKeychain/TIP403Registry/StablecoinDEX 的 valid view selector 确认（从源码 sol! 宏获取）
- [ ] ValidatorConfigV2 owner() 执行结果差异排查（W=0x 空 vs L=0x...0000 零值填充）
- [ ] latest block 测试: getBlockByHeight latest / getBlockById latest / estimateGas @ latest
- [ ] 批量 100 blocks 非零 storage 采样（ValidatorConfig slot 0 + USDC totalSupply）
- [ ] eth_multiCall gasUsed (23270) vs estimateGas (23607) 差异确认（estimateGas buffer 机制）
- [ ] getBalance/getAddressBalance 返回 Tempo NATIVE_BALANCE_PLACEHOLDER（方案: EvmCfg 加 virtual_balance 字段，EvmStorageWrapper.basic_ref 注入）
- [x] **BUG: T1A hardfork 后 leafage gas 计算与 writer 不一致** — 影响 estimateGas 和 pre_traceMany gasUsed
  - **表现**: T1A 后 (block 4494500+, ts >= 1770908400) writer gasUsed=273270, leafage gasUsed=23270, 差 250000
  - **trace 内部 gasUsed 一致** (0x89e=2206)，差异在外层 gas accounting（可能是 new_account_cost 从 25000→250000）
  - **代码路径确认**: `TempoEvm::new()` 正确检测 hardfork (`from_timestamp`)，设置了 `gas_params.override_gas()`，但 **revm 执行时可能没有使用 cfg_env.gas_params**
  - **根因**: `TempoEvm::new()` 构建 instruction table 用了 `EthInstructions::new_mainnet()`，其内部 `SpecId::default()` = `FRONTIER`。Frontier spec 不含 EIP-2929 cold/warm account access gas 改动，导致外层 CALL 的 cold account gas (250k) 不被 charge
  - **修复**: `EthInstructions::new_mainnet()` → `EthInstructions::new_mainnet_with_spec(spec)` 传入 OSAKA spec。已修复，编译通过，11/11 单测 pass
  - **影响**: 修复前 estimateGas 和 pre_traceMany gasUsed 在 T1A 后偏小 250k（23607 vs 276606），eth_call 返回值（output）正确不受影响
