# Tempo 链适配 leafage-evm 技术方案

## 1. 背景

Tempo 是 Stripe + Paradigm 联合孵化的支付专用 L1（Chain ID 4217，2026-03-18 主网上线），基于 Reth SDK 构建。DeBankCore 需要通过 leafage-evm 对 Tempo 链进行状态查询和交易预执行。

leafage-evm 是 DeBank 的通用 EVM 读节点，消费 pipeline 数据（state_diff）重建链上 state，对外提供 `eth_call`、`estimateGas`、`simulateTransactions` 等 RPC。当前已支持 Ethereum mainnet、BSC、OP Stack、Cosmos EVM、Mantle 五类链。

## 2. 数据流

```
Tempo writer 节点 (trace_debankBlock RPC)
       ↓ HTTP
background-tracer (sidecar)
       ↓                    ↓
   S3 上传                Kafka 发布
  (state_diff/header/     (BlockChangeNotify)
   blockFile/validation)
       ↓                    ↓
   leafage-evm            DeBankCore
  (state query)         (blockfile traces)
```

- **Writer 端**：DeBankDeFi/tempo fork（`~/code/task_tempo/`，branch: debank），在 Tempo 节点内新增 `trace_debankBlock` RPC，输出 block_file + state_diff + header
- **Pipeline**：`~/ghorg/chaintable/pipeline/`，Kafka/S3 中转
- **Leafage**：本项目，消费 state_diff 重建 EVM state，对外提供 RPC

## 3. Tempo 与标准 EVM 的关键差异

| 差异点 | 标准 EVM | Tempo | 对 leafage 的影响 |
|--------|---------|-------|-----------------|
| Native token | ETH | 无（gas 以 TIP-20 USDC/USDT 支付） | BALANCE/SELFBALANCE 返回 0，无需处理（state 自然为 0） |
| Gas 参数 | SSTORE 20k, CREATE 32k | SSTORE 250k, CREATE 500k (TIP-1000) | **已适配** — 升级 revm 36.0，通过 `GasParams::override_gas()` 注入 7 项 TIP-1000 gas override |
| 预编译 | 标准以太坊 (0x01-0x0a) | 标准 + 9 个 Tempo 自定义预编译 | **已适配** — 全部 9 个预编译已移植 |
| 交易类型 | Legacy/EIP-1559/4844/7702 | 新增 TempoTransaction (type 0x76) — AA tx | **已适配** — 批量原子执行 + 2D nonce，fee/签名 eth_call 模式自动跳过 |
| Fee 机制 | ETH gas fee | TIP-20 fee + AMM 兑换 | **无需实现** — Tempo writer 的 `eth_call` / `eth_estimateGas` 在 `disable_balance_check=true` 下，handler 的 `validate_against_state_and_deduct_caller` 中 `calculate_caller_fee` 返回的 `new_balance >= account_balance`，导致 `gas_balance_spending=0` 短路跳过 `collect_fee_pre_tx`。leafage 行为一致 |
| Value transfer | 允许 | 禁止 | 无需处理（DeBankCore 发 value=0） |
| Hardfork | EIP 编号 | T0→T3，全部映射到 SpecId::OSAKA | 只跑最新 spec |

## 4. 架构设计

### 4.1 遵循 leafage-evm 链适配模式

```
crates/leafage-evm-chains/src/tempo/     ← 链特化代码
├── mod.rs                                 module root
├── hardfork.rs                            最小 TempoHardfork 枚举
├── gas_params.rs                          TIP-1000 gas 常量
├── precompile/                            9 个预编译 + storage 层
│   ├── mod.rs                             注册入口 + Precompile trait
│   ├── storage.rs                         StorageCtx + LeafageStorageProvider
│   ├── storage_types.rs                   Slot<T> / Mapping<K,V> / packing
│   ├── error.rs                           TempoPrecompileError
│   ├── tip20.rs                           TIP20Token (核心)
│   ├── tip20_factory.rs                   TIP20Factory
│   ├── tip403_registry.rs                 TIP403Registry
│   ├── fee_manager.rs                     TipFeeManager + AMM
│   ├── stablecoin_dex.rs                  StablecoinDEX
│   ├── nonce.rs                           NonceManager
│   ├── account_keychain.rs                AccountKeychain
│   ├── validator_config.rs                ValidatorConfig
│   └── validator_config_v2.rs             ValidatorConfigV2
└── api/
    ├── mod.rs                             TempoEvm wrapper
    └── exec.rs                            ExecuteEvm / InspectCommitEvm

crates/leafage-evm-rpc/src/api_impl/tempo/  ← RPC 接入层
├── mod.rs
└── api.rs                                 TempoApiImpl (EvmExecutor impl)
```

### 4.2 执行路径

```
DeBankCore 调用 simulateTransactions / estimateGas
  ↓
DebankApiServer (debank.rs) — 通用 RPC 入口
  ↓
EvmExecutor trait dispatch — 由 MultiChainCfgEnv::Tempo 路由
  ↓
TempoApiImpl::transact() / inspect_tx_commit()
  ↓
TempoEvm::new() — 创建 EVM 实例
  ├── 标准以太坊预编译 (EthPrecompiles, SpecId::OSAKA)
  └── extend_tempo_precompiles() — 注册 9 个 Tempo 预编译
        └── set_precompile_lookup() — 动态地址分派
  ↓
revm 执行 — 标准 EthHandler (无自定义 handler)
  ↓
遇到 0x20C0... 地址 → TIP20 预编译
  ├── dispatch_call() 按 4-byte selector 路由
  ├── balanceOf() → StorageCtx::sload(addr, keccak256(user . slot_9))
  └── 读取 leafage StateTree 中的 state
```

### 4.3 预编译 storage 访问原理

Tempo 的 9 个预编译没有 EVM bytecode，所有数据编码在 EVM storage slots 中（通过 `#[contract]` 宏生成 Solidity 兼容 slot 布局）。

```
预编译业务逻辑 (TIP20.balanceOf 等)
    ↓ 调用 Slot<T> / Mapping<K,V> 类型
StorageCtx (scoped thread-local, 转发到 provider)
    ↓ sload(address, slot) / sstore(address, slot, value)
LeafageStorageProvider (包装 alloy-evm 0.29.2 的 EvmInternals)
    ↓
revm Journal → leafage StateTree (pipeline 同步的 state)
```

示例 — TIP20 `balanceOf(user)`:
```
1. 预编译地址: 0x20C0000000000000000000000000000000000000 (pathUSD)
2. slot 计算: keccak256(abi_encode(user_address) ++ abi_encode(9))  // 9 = balances mapping 的 base slot
3. SLOAD(0x20C0...0000, computed_slot) → 从 leafage StateTree 读取
4. 返回 U256 balance
```

### 4.4 动态地址分派

Tempo 的 TIP20 预编译地址以 `0x20C0` 为前缀（12 bytes），每个 TIP20 token 有独立地址。不能像 BSC 那样枚举注册。

使用 `PrecompilesMap::set_precompile_lookup()` 实现前缀匹配：

```rust
precompiles.set_precompile_lookup(move |address: &Address| {
    if is_tip20_prefix(*address) { Some(create_tip20_precompile(*address, chain_id)) }
    else if *address == TIP_FEE_MANAGER_ADDRESS { Some(...) }
    else if *address == STABLECOIN_DEX_ADDRESS { Some(...) }
    // ... 其余 7 个精确地址匹配
    else { None }
});
```

### 4.5 revm 版本

leafage 已升级到 revm 36.0 / alloy-evm 0.29.2，与 Tempo writer 端版本一致。

升级依赖链：
| crate | 升级前 | 升级后 |
|-------|--------|--------|
| revm | 33.1.0 | 36.0.0 |
| op-revm | 14.1.0 | 17.0.0 |
| revm-inspectors | 0.33.2 | 0.36.1 |
| alloy-evm | 0.25.2 | 0.29.2 |
| revm-bytecode | 7.1.1 | 9.0.0 |
| Rust MSRV | 1.79 | 1.91 |

Tempo TIP-1000 gas 参数通过 `GasParams::override_gas()` 原生注入，7 项 override：
- `sstore_set_without_load_cost` → 250,000
- `create` / `tx_create_cost` → 500,000
- `new_account_cost` / `new_account_cost_for_selfdestruct` → 250,000
- `code_deposit_cost` → 1,000/byte
- `tx_eip7702_per_empty_account_cost` → 12,500

仍保留的适配点：
| 项 | 说明 |
|---|---|
| `TempoHardfork` | 最小枚举 (~30 行)，所有 `is_*()` 返回 true。不依赖 `tempo-chainspec` |
| `LeafageStorageProvider` | 包装 `EvmInternals` 为预编译提供 storage 访问 |
| Journal checkpoint | stub（leafage 只读场景不需要） |

## 5. 预编译地址表

| 预编译 | 地址 | 行数 | 说明 |
|--------|------|------|------|
| TIP20Token | `0x20C0...` (前缀匹配) | 1823 | 核心 — ERC-20 兼容 token 标准 |
| TIP20Factory | `0x20FC000000000000000000000000000000000000` | 353 | Token 创建工厂 |
| TIP403Registry | `0x403C000000000000000000000000000000000000` | 949 | 合规策略注册表 (白/黑名单) |
| TipFeeManager | `0xfeec000000000000000000000000000000000000` | 985 | Fee token 偏好 + AMM |
| StablecoinDEX | `0xdec0000000000000000000000000000000000000` | 2239 | CLOB 订单簿 |
| NonceManager | `0x4E4F4E4345000000000000000000000000000000` | 280 | 2D nonce 存储 |
| AccountKeychain | `0xAAAAAAAA00000000000000000000000000000000` | 715 | 账户密钥管理 |
| ValidatorConfig | `0xCCCCCCCC00000000000000000000000000000000` | 665 | 验证者配置 V1 |
| ValidatorConfigV2 | `0xCCCCCCCC00000000000000000000000000000001` | 1377 | 验证者配置 V2 (T1C+) |

## 6. 当前完成状态

**分支:** `feature/tempo-chain-adaptation` (12 commits)

### 已完成

| 组件 | 状态 | 说明 |
|------|------|------|
| 模块脚手架 | done | hardfork 枚举 + gas 常量 |
| Storage 适配层 | done | LeafageStorageProvider + StorageCtx + 类型系统 |
| 9 个预编译 | done | 全部移植，编译通过 |
| TempoEvm wrapper | done | 动态预编译注册 + smoke test 通过 |
| RPC 接入 | done | TempoApiImpl + MultiChainCfgEnv::Tempo |
| CLI 入口 | done | `--evm-type tempo --chain-cfg 4217` |
| revm 升级 | done | 33.1→36.0，TIP-1000 gas 通过 `GasParams` 原生注入 |
| 0x76 batch execution | done | TempoTxEnv + TempoHandler multi-call + 2D nonce + CallRequest 扩展 |
| 全量编译 | done | `cargo check --workspace` 通过 (Rust 1.93.0)，3 个 test 通过 |

### 已知 stub（预编译间交叉调用未连接）

| 项 | 状态 | 影响 |
|---|------|------|
| ~~TIP20 → TIP403 compliance check~~ | **已连接** | transfer/mint/burnBlocked 全部经 TIP403 合规检查 |
| ~~TIP20 → AccountKeychain spending limits~~ | **已连接** | transfer/approve/distributeReward 经 AccountKeychain 限额检查 |
| ~~FeeManager → TIP20 token transfer~~ | **已连接** | collect_fee_pre/post_tx + AMM (rebalance_swap/mint/burn) 全部调 TIP20 |
| ~~FeeManager → TIP20Factory::is_tip20~~ | **已连接** | set_validator_token / set_user_token 调 TIP20Factory |
| ~~TIP20 → TIP20Factory validation~~ | **已连接** | set_next_quote_token 调 is_tip20() + currency 验证 + cycle detection |
| StablecoinDEX → TIP20 token transfer | stub | 无影响 — view 方法可正确读链上状态 |
| ed25519 / P256 / WebAuthn 签名验证 | 无需实现 | eth_call 不触发签名验证 |
| TIP20 permit ecrecover | 无法实现 | leafage 无 ecrecover 访问，permit 是写操作 |
| Journal checkpoint (预编译内部) | stub | TempoHandler 级别已通过 revm journal 实现批量原子性 |

## 7. 已知限制

### ~~7.1 EVM opcode gas 计费不准确~~ (已解决)

已通过升级 revm 36.0 + `GasParams::override_gas()` 解决。TIP-1000 的 7 项 gas override 现在在 EVM 运行时生效，`estimateGas` 和 `simulateTransactions` 的 gas 计算与 Tempo 链一致。

### 7.1 estimateGas 与 writer 端差异

| 差异点 | Tempo writer (eth_estimateGas) | leafage (estimateGas) | 影响 |
|--------|-------------------------------|----------------------|------|
| gas 上界 | TIP-20 余额 * SCALING_FACTOR / gas_price (`caller_gas_allowance`) | rpc_gas_cap (固定值，默认 100M) | 无实际影响 — reth 在 `disable_balance_check` 时 fallback 到 block_gas_limit |
| fee handler | 短路跳过（`gas_balance_spending=0`） | 同样不执行 | 无差异 |
| EVM 执行 | TempoEvmHandler（标准 EVM 执行，fee 已短路） | EthHandler（标准 EVM 执行） | gas 计算一致（TIP-1000 已通过 GasParams 注入） |
| 2D nonce | `create_txn_env` 从 NonceManager storage 读取 | 使用 account nonce | 无实际影响 — eth_call 跳过 nonce 检查，estimateGas 在 `disable_balance_check` 下同样跳过 |

### ~~7.2 TempoTransaction (type 0x76) 批量执行~~ (已实现)

已完整实现。通过分析 Tempo handler 源码确认 eth_call 模式下 fee handler 和签名验证自动跳过。

实现：
- `TempoTxEnv` — 包装 `TxEnv` + `TempoTxFields` (aa_calls, nonce_key)
- `TempoHandler::execution()` — override 分发 batch/single call，batch 用 journal checkpoint 实现原子性
- `CallRequest` — 扩展为 wrapper struct（`Deref`/`DerefMut` 到 `TransactionRequest`），新增 `tempo_calls` / `nonce_key` 可选字段，JSON 向后兼容
- `create_txn_env` — 读取 `tempo_calls` / `nonce_key` 填充 `TempoTxFields`

DeBankCore 调用方式：
```json
{
  "from": "0xabc...",
  "to": "0x20C0...",
  "data": "0x...",
  "tempo_calls": [
    {"to": "0x20C0...", "data": "0xa9059cbb...", "value": "0x0"},
    {"to": "0x20C0...", "data": "0xa9059cbb...", "value": "0x0"}
  ],
  "nonce_key": "0x1"
}
```

注意：对于 DeBankCore 的模拟交易和 gas 预估场景，用现有 `Vec<CallRequest>` 逐笔模拟通常已够用。batch 执行仅在需要原子性语义时有差异。

### 7.4 Fee log 不产生

与 writer 端 `pre_traceMany` 行为一致 — `simulateTransactions` 不执行 fee handler，不产生 TIP-20 fee Transfer log。

## 8. 后续工作

按优先级排列：

### P0 — 上线前必须

- [ ] **集成测试** — 对照 dev 环境（blockchain-misc-x3, 端口 8566）验证 TIP20 balanceOf/transfer, eth_multiCall, simulateTransactions, estimateGas
- [x] ~~**Cross-precompile 连接**~~ — TIP20 ↔ TIP403 和 TIP20 ↔ AccountKeychain 已全部连接

### P1 — 上线后优化

- [x] ~~**revm gas 参数修正**~~ — 已完成，升级 revm 36.0 + GasParams
- [x] ~~**estimateGas fee overhead**~~ — 已确认无差异，writer 端 eth_estimateGas 在 `disable_balance_check` 下同样短路 fee handler

### P2 — 按需

- [x] ~~**TempoTransaction (0x76) 批量执行**~~ — 已完成，TempoTxEnv + TempoHandler + CallRequest 扩展
- [ ] **Fee log 生成** — 如 DeBankCore 需要 fee Transfer log
- [ ] **Hardfork 动态切换** — 如需 archive 模式支持历史区块
- [ ] **cargo feature gate** — `tempo` feature 减少非 Tempo 链编译时间

## 9. 启动参数

```bash
leafage-evm standalone \
  --evm-type tempo \
  --chain-cfg 4217 \
  --db-path /data/leafage-tempo \
  --kafka-s3-config /path/to/config.json \
  --listen-addr 0.0.0.0:8545
```

## 10. 参考文档

| 文档 | 位置 |
|------|------|
| 实现计划 | `docs/superpowers/plans/2026-03-25-tempo-chain-adaptation.md` |
| TODO / 决策记录 | `docs/todo.md` |
| Tempo writer 端设计 | `~/code/task_tempo/docs/debank-rpc-design.md` |
| Tempo 全栈改造分析 | `~/code/task_tempo/docs/tempo-reth-customizations.md` |
| Writer 端测试报告 | `~/code/task_tempo/docs/test-report.md` |
| 通用节点方案 | `~/code/task_tempo/docs/generic-node.md` |
