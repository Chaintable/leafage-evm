# Tempo Chain Adaptation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Tempo chain support to leafage-evm so DeBankCore can query Tempo state via `estimateGas` and `simulateTransactions`, with all 9 Tempo precompiles.

**Architecture:** Port Tempo's precompile crates (`~19k LOC` across 9 precompiles + `~8k LOC` storage/macro layer) into a new `leafage-evm-chains/src/tempo/` module. Use `PrecompilesMap::set_precompile_lookup()` for dynamic TIP20 address dispatch. Wrap in `TempoEvm` following the BSC pattern. No AA tx handler, no fee handler — standard `TxEnv` with Tempo precompiles + gas params.

**Tech Stack:** Rust, revm 33.1.0, alloy-evm 0.25.2, alloy 1.1.3, jsonrpsee 0.24

**Tempo source reference:** `~/code/task_tempo/crates/precompiles/` (precompile impls), `~/code/task_tempo/crates/precompiles-macros/` (storage layout macros), `~/code/task_tempo/crates/contracts/src/precompiles/mod.rs` (addresses)

---

## Critical: revm 33.1 vs 36 Adaptation Strategy

Tempo 预编译代码基于 revm 36 / alloy-evm 0.29。leafage 用 revm 33.1 / alloy-evm 0.25.2，以下 API 不存在，需要显式适配：

### 1. `GasParams` / `GasId` 不存在于 revm 33.1

**策略:** 不使用 `GasParams` API。创建 `TempoGasCosts` struct，hardcode TIP-1000 gas 常量。所有预编译中 `self.gas_params.get(GasId::xxx)` 替换为 `TempoGasCosts::XXX` 常量访问。

```rust
pub struct TempoGasCosts;
impl TempoGasCosts {
    pub const SSTORE_SET: u64 = 250_000;
    pub const CREATE: u64 = 500_000;
    pub const NEW_ACCOUNT: u64 = 250_000;
    pub const CODE_DEPOSIT_PER_BYTE: u64 = 1_000;
    pub const AUTH_ACCOUNT_CREATION: u64 = 250_000;
    // ... 其余从 tempo gas_params.rs 提取
}
```

### 2. `EvmInternals` 构造函数差异

- alloy-evm 0.25.2: `EvmInternals::new(journal, block_env)` — 2 参数，无 `chain_id()`, 无 `load_account_mut_skip_cold_load()`
- alloy-evm 0.29: `EvmInternals::new(journal, block_env, cfg_env, tx_env)` — 4 参数

**策略:** 创建 `LeafageStorageProvider` 包装 `EvmInternals<'a>`，将缺失数据（`chain_id`, gas costs）作为构造参数显式传入，而非从 `EvmInternals` 获取。`load_account_mut_skip_cold_load` 用 `load_account` + 手动 cold-check 逻辑替代。

### 3. `TempoHardfork` 类型替换

预编译代码大量使用 `spec.is_t1()`, `spec.is_t1c()` 分支。

**策略:** 创建最小 `TempoHardfork` 枚举（~30 行），hardcode 到最新 hardfork（所有 `is_t1()` / `is_t1c()` 返回 `true`）。不依赖 `tempo-chainspec` crate。

### 4. `tempo_precompile!` 宏重写

原始宏依赖 `EvmPrecompileStorageProvider::new(input.internals, gas_limit, spec, is_static, gas_params)`。需要适配为 `LeafageStorageProvider::new(input.internals, gas_limit, chain_id, is_static)`。

---

## File Structure

### New files in `crates/leafage-evm-chains/src/tempo/`

| File | Responsibility | Ported from (task_tempo) |
|------|---------------|--------------------------|
| `mod.rs` | Module root, re-exports | — |
| `hardfork.rs` | Minimal `TempoHardfork` enum (latest-only, ~30 行) | `crates/chainspec/src/hardfork.rs` (simplified) |
| `gas_params.rs` | `TempoGasCosts` hardcoded constants | `crates/revm/src/gas_params.rs` |
| `precompile/mod.rs` | `extend_tempo_precompiles()`, address constants, `tempo_precompile!` macro | `crates/precompiles/src/lib.rs` + `crates/contracts/src/precompiles/mod.rs` |
| `precompile/storage.rs` | `PrecompileStorageProvider` trait, `LeafageStorageProvider`, `StorageCtx` | `crates/precompiles/src/storage/` (evm.rs, thread_local.rs, mod.rs) |
| `precompile/storage_types.rs` | Slot/mapping/array/vec storage type helpers, packing | `crates/precompiles/src/storage/types/`, `storage/packing.rs` |
| `precompile/error.rs` | `TempoPrecompileError`, `Result`, helper traits | `crates/precompiles/src/error.rs` |
| `precompile/tip20.rs` | TIP20Token precompile + dispatch | `crates/precompiles/src/tip20/` |
| `precompile/tip20_factory.rs` | TIP20Factory precompile + dispatch | `crates/precompiles/src/tip20_factory/` |
| `precompile/tip403_registry.rs` | TIP403Registry precompile + dispatch | `crates/precompiles/src/tip403_registry/` |
| `precompile/fee_manager.rs` | TipFeeManager precompile + dispatch (+ AMM) | `crates/precompiles/src/tip_fee_manager/` |
| `precompile/stablecoin_dex.rs` | StablecoinDEX precompile + dispatch | `crates/precompiles/src/stablecoin_dex/` |
| `precompile/nonce.rs` | NonceManager precompile + dispatch | `crates/precompiles/src/nonce/` |
| `precompile/account_keychain.rs` | AccountKeychain precompile + dispatch | `crates/precompiles/src/account_keychain/` |
| `precompile/validator_config.rs` | ValidatorConfig precompile + dispatch | `crates/precompiles/src/validator_config/` |
| `precompile/validator_config_v2.rs` | ValidatorConfigV2 precompile + dispatch | `crates/precompiles/src/validator_config_v2/` |
| `api/mod.rs` | `TempoEvm` wrapper (like `BscEvm`) | BSC pattern + Tempo precompile registration |
| `api/exec.rs` | `ExecuteEvm` / `InspectCommitEvm` impls for `TempoEvm` | BSC `exec.rs` pattern |

### New files in `crates/leafage-evm-rpc/src/api_impl/tempo/`

| File | Responsibility |
|------|---------------|
| `mod.rs` | Module declaration |
| `api.rs` | `TempoApiImpl` — `EvmExecutor` impl, error mappings, `ApiCore` |

### Modified files

| File | Change |
|------|--------|
| `crates/leafage-evm-chains/src/lib.rs` | Add `pub mod tempo;` |
| `crates/leafage-evm-chains/Cargo.toml` | Add `scoped-tls` + 检查 crypto deps |
| `crates/leafage-evm-rpc/src/api_impl/mod.rs` | Add `mod tempo;` |
| `crates/leafage-evm-rpc/src/api_impl/core.rs` | Add `MultiChainCfgEnv::Tempo` variant |
| `crates/leafage-evm-rpc/src/api_impl/build.rs` | Add `MultiChainCfgEnv::Tempo` match arm |
| `bin/leafage-evm/src/standalone.rs` | Add `"tempo"` evm_type + `parse_chain_cfg` |

---

## Tasks

### Task 1: Scaffold + hardfork enum + gas costs

**Files:**
- Create: `crates/leafage-evm-chains/src/tempo/mod.rs`
- Create: `crates/leafage-evm-chains/src/tempo/hardfork.rs`
- Create: `crates/leafage-evm-chains/src/tempo/gas_params.rs`
- Modify: `crates/leafage-evm-chains/src/lib.rs`
- Modify: `crates/leafage-evm-chains/Cargo.toml`

- [ ] **Step 1: Create module scaffold**

`tempo/mod.rs`:
```rust
pub mod hardfork;
pub mod gas_params;
pub mod precompile;
pub mod api;
```

`lib.rs` 加 `pub mod tempo;`

- [ ] **Step 2: Create minimal TempoHardfork enum**

`tempo/hardfork.rs` — 从 `~/code/task_tempo/crates/chainspec/src/hardfork.rs` 精简。只保留枚举定义和 `is_*()` 方法，hardcode 到最新（所有返回 `true`）。~30 行。

- [ ] **Step 3: Create TempoGasCosts**

`tempo/gas_params.rs` — 从 `~/code/task_tempo/crates/revm/src/gas_params.rs` 提取所有 TIP-1000 常量值，存为 `pub const`。不依赖 `GasParams` API。

- [ ] **Step 4: Add `scoped-tls` to Cargo.toml**

- [ ] **Step 5: Verify compilation**

Run: `cargo check -p leafage-evm-chains 2>&1 | tail -5`

- [ ] **Step 6: Commit**

```
feat(tempo): scaffold tempo module with hardfork enum and gas costs
```

---

### Task 2: Port precompile storage layer + API adaptation shim

这是最关键的适配层。所有预编译依赖此基础设施。

**Files:**
- Create: `crates/leafage-evm-chains/src/tempo/precompile/mod.rs` (空壳 + 地址常量)
- Create: `crates/leafage-evm-chains/src/tempo/precompile/storage.rs`
- Create: `crates/leafage-evm-chains/src/tempo/precompile/storage_types.rs`
- Create: `crates/leafage-evm-chains/src/tempo/precompile/error.rs`

**Source reference:**
- `~/code/task_tempo/crates/precompiles/src/storage/` (~4710 行)
- `~/code/task_tempo/crates/precompiles/src/error.rs`

- [ ] **Step 1: Port error types**

从 `error.rs` 移植 `TempoPrecompileError`, `Result<T>`, `IntoPrecompileResult` trait。适配 revm 33.1 的 `PrecompileResult` / `PrecompileOutput` 类型（确认签名差异）。

- [ ] **Step 2: Port PrecompileStorageProvider trait + StorageCtx**

从 `storage/mod.rs` 移植 `PrecompileStorageProvider` trait（去掉 `GasParams` 相关参数）。
从 `storage/thread_local.rs` 移植 `StorageCtx`（scoped_thread_local 模式，依赖 `scoped-tls`）。

- [ ] **Step 3: Build LeafageStorageProvider (适配 alloy-evm 0.25.2)**

创建 `LeafageStorageProvider`，替代 Tempo 的 `EvmPrecompileStorageProvider`：
- 包装 `EvmInternals<'a>` (alloy-evm 0.25.2 版本)
- 构造函数: `new(internals, gas_limit, chain_id, is_static)` — 显式传入 `chain_id`
- `sload()` / `sstore()`: 通过 `EvmInternals` 的 journal 操作，gas 计费用 `TempoGasCosts` 常量
- `load_account`: 用 `EvmInternals::load_account()` 替代 `load_account_mut_skip_cold_load()`
- `emit_log()` / `checkpoint()` / `revert()`: 适配 alloy-evm 0.25.2 journal API

这是最需要仔细对比 API 差异的步骤。参考 `~/code/task_tempo/crates/precompiles/src/storage/evm.rs` (682 行) 逐方法适配。

- [ ] **Step 4: Port storage type helpers**

从 `storage/types/` 移植（纯计算逻辑，不依赖 revm 版本）:
- `slot.rs` — `StorageSlot<T>` 读写单个 slot
- `mapping.rs` — `StorageMapping<K, V>` keccak256 slot 计算
- `primitives.rs` — U256/Address/bool/u64 等的 slot 编解码
- `packing.rs` — 多字段共享 slot 的 bit packing

- [ ] **Step 5: 创建 tempo_precompile! 宏 (适配版)**

在 `precompile/mod.rs` 中定义适配版宏，使用 `LeafageStorageProvider` 替代原始构造:
```rust
macro_rules! tempo_precompile {
    ($chain_id:expr, |$input:ident| $impl:expr) => {{
        DynPrecompile::new_stateful(move |$input| {
            if !$input.is_direct_call() {
                return Ok(PrecompileOutput::new_reverted(0, ...));
            }
            let mut storage = LeafageStorageProvider::new(
                $input.internals,
                $input.gas,
                $chain_id,
                $input.is_static,
            );
            StorageCtx::enter(&mut storage, || $impl.call($input.data, $input.caller))
        })
    }};
}
```

- [ ] **Step 6: 添加地址常量到 `precompile/mod.rs`**

```rust
pub const TIP_FEE_MANAGER_ADDRESS: Address = address!("0xfeec000000000000000000000000000000000000");
pub const TIP20_FACTORY_ADDRESS: Address = address!("0x20FC000000000000000000000000000000000000");
// ... 全部 9 个地址
```

- [ ] **Step 7: Verify compilation**

- [ ] **Step 8: Commit**

```
feat(tempo): port precompile storage layer with alloy-evm 0.25.2 adaptation shim
```

---

### Task 3: Port TIP20 precompile

TIP20 是最核心的预编译（2976 行 + dispatch），DeBankCore 直接调用。

**Files:**
- Create: `crates/leafage-evm-chains/src/tempo/precompile/tip20.rs`

**Source reference:**
- `~/code/task_tempo/crates/precompiles/src/tip20/mod.rs` (2976 行)
- `~/code/task_tempo/crates/precompiles/src/tip20/dispatch.rs`

- [ ] **Step 1: Port TIP20Token struct + storage layout**

`#[contract]` 宏展开后的 slot 布局（用 `cargo expand` 或读宏源码推算）。需要手动实现每个字段的 `StorageSlot` / `StorageMapping` 定义。核心字段: `balances`, `allowances`, `total_supply`, `name`, `symbol`, `decimals`, `currency`, roles 等。

- [ ] **Step 2: Port `is_tip20_prefix()` 和 `TIP20Token::from_address()`**

- [ ] **Step 3: Port dispatch (Precompile trait impl)**

从 `dispatch.rs` 移植 selector 路由 + 从 `mod.rs` 移植各方法实现。

- [ ] **Step 4: 验证编译**

- [ ] **Step 5: Commit**

```
feat(tempo): port TIP20 precompile
```

---

### Task 4a: Port small precompiles (NonceManager + FeeManager + TIP20Factory + ValidatorConfig)

**Files:**
- Create: `precompile/nonce.rs` (458 行)
- Create: `precompile/fee_manager.rs` (847 行 + amm.rs)
- Create: `precompile/tip20_factory.rs` (870 行)
- Create: `precompile/validator_config.rs` (1065 行)

- [ ] **Step 1: Port NonceManager** — 2D nonce 存储 + 过期 nonce 环形缓冲区
- [ ] **Step 2: Port TipFeeManager** — Fee token 偏好存储 + AMM 子模块
- [ ] **Step 3: Port TIP20Factory** — 代币创建工厂，`is_tip20()` 全量验证
- [ ] **Step 4: Port ValidatorConfig** — 验证者配置读取
- [ ] **Step 5: 验证编译**
- [ ] **Step 6: Commit**

```
feat(tempo): port NonceManager, FeeManager, TIP20Factory, ValidatorConfig precompiles
```

---

### Task 4b: Port medium precompiles (AccountKeychain + TIP403Registry)

**Files:**
- Create: `precompile/account_keychain.rs` (1887 行)
- Create: `precompile/tip403_registry.rs` (2256 行)

- [ ] **Step 1: Port AccountKeychain** — 账户密钥管理。检查 P256/WebAuthn 签名验证的 crypto 依赖 — 如果 leafage eth_call 不触发签名验证路径，可以 stub 这些方法（返回 error）
- [ ] **Step 2: Port TIP403Registry** — 合规策略注册表。TIP20 transfer 内部会调用 TIP403 检查
- [ ] **Step 3: 验证编译**
- [ ] **Step 4: Commit**

```
feat(tempo): port AccountKeychain and TIP403Registry precompiles
```

---

### Task 4c: Port large precompiles + registration (ValidatorConfigV2 + StablecoinDEX)

**Files:**
- Create: `precompile/validator_config_v2.rs` (3630 行)
- Create: `precompile/stablecoin_dex.rs` (4952 行)
- Modify: `precompile/mod.rs` — 注册所有预编译

- [ ] **Step 1: Port ValidatorConfigV2** — 升级版验证者配置（T1C+）
- [ ] **Step 2: Port StablecoinDEX** — CLOB 订单簿（最大的预编译）。检查是否有 `ip_validation` 模块依赖，如有则一并移植
- [ ] **Step 3: 实现 `extend_tempo_precompiles()`**

在 `precompile/mod.rs` 中注册所有 9 个预编译:
```rust
pub fn extend_tempo_precompiles(precompiles: &mut PrecompilesMap, chain_id: u64) {
    precompiles.set_precompile_lookup(move |address: &Address| {
        if is_tip20_prefix(*address) { Some(create_tip20_precompile(*address, chain_id)) }
        else if *address == TIP20_FACTORY_ADDRESS { Some(create_tip20_factory_precompile(chain_id)) }
        else if *address == TIP403_REGISTRY_ADDRESS { Some(create_tip403_precompile(chain_id)) }
        else if *address == TIP_FEE_MANAGER_ADDRESS { Some(create_fee_manager_precompile(chain_id)) }
        else if *address == STABLECOIN_DEX_ADDRESS { Some(create_dex_precompile(chain_id)) }
        else if *address == NONCE_PRECOMPILE_ADDRESS { Some(create_nonce_precompile(chain_id)) }
        else if *address == VALIDATOR_CONFIG_ADDRESS { Some(create_validator_config_precompile(chain_id)) }
        else if *address == ACCOUNT_KEYCHAIN_ADDRESS { Some(create_keychain_precompile(chain_id)) }
        else if *address == VALIDATOR_CONFIG_V2_ADDRESS { Some(create_validator_config_v2_precompile(chain_id)) }
        else { None }
    });
}
```

- [ ] **Step 4: 验证全部预编译编译通过**

Run: `cargo check -p leafage-evm-chains 2>&1 | tail -10`

- [ ] **Step 5: Commit**

```
feat(tempo): port ValidatorConfigV2, StablecoinDEX and register all precompiles
```

---

### Task 5: Build TempoEvm wrapper + smoke test

**Files:**
- Create: `crates/leafage-evm-chains/src/tempo/api/mod.rs`
- Create: `crates/leafage-evm-chains/src/tempo/api/exec.rs`

**Pattern reference:** `crates/leafage-evm-chains/src/bsc/api/mod.rs` (178 行) + `exec.rs` (79 行)

- [ ] **Step 1: Define TempoEvm struct**

```rust
pub type TempoContext<DB> = Context<BlockEnv, TxEnv, CfgEnv<MainnetSpecId>, DB>;

pub struct TempoEvm<DB: Database, I> {
    pub inner: EvmCtx<TempoContext<DB>, I, EthInstructions<...>, PrecompilesMap, EthFrame>,
    pub inspect: bool,
}
```

- [ ] **Step 2: Implement `TempoEvm::new()`**

构造时注册 Tempo 预编译:
```rust
pub fn new(env: EvmEnv<MainnetSpecId>, db: DB, inspector: I, inspect: bool) -> Self {
    let mut precompiles = PrecompilesMap::from_static(
        EthPrecompiles::new(env.cfg_env.spec).precompiles
    );
    extend_tempo_precompiles(&mut precompiles, env.cfg_env.chain_id);
    // ... build EvmCtx
}
```

- [ ] **Step 3: Implement EvmTr + InspectorEvmTr** — delegate to `self.inner`

- [ ] **Step 4: Implement ExecuteEvm + InspectCommitEvm** — 使用 `EthHandler`

- [ ] **Step 5: Smoke test**

添加 `#[test]`:
- 构造 `TempoEvm` with `EmptyDB`，执行 trivial call 到非预编译地址 — 不 panic
- 调用 TIP20 地址（`0x20C0...` + `totalSupply` selector）— 应返回结果（即使 EmptyDB 无状态数据）

- [ ] **Step 6: 验证编译 + 测试通过**

Run: `cargo test -p leafage-evm-chains --lib tempo 2>&1 | tail -10`

- [ ] **Step 7: Commit**

```
feat(tempo): add TempoEvm wrapper with precompile dispatch and smoke test
```

---

### Task 6: Wire up TempoApiImpl + MultiChainCfgEnv

**Files:**
- Create: `crates/leafage-evm-rpc/src/api_impl/tempo/mod.rs`
- Create: `crates/leafage-evm-rpc/src/api_impl/tempo/api.rs`
- Modify: `crates/leafage-evm-rpc/src/api_impl/mod.rs`
- Modify: `crates/leafage-evm-rpc/src/api_impl/core.rs`
- Modify: `crates/leafage-evm-rpc/src/api_impl/build.rs`

- [ ] **Step 1: Define TempoApiImpl**

```rust
type TempoApiImpl<DB> = ApiImpl<DB, MainnetSpecId, NoneEvmCustomConfig>;
```

注意: 与 mainnet 的 `MainnetApiImpl` 类型相同，但 `EvmExecutor` impl 不同（用 `TempoEvm`）。需要区分 — 考虑用 marker type 或独立 struct。

- [ ] **Step 2: Implement EvmExecutor for TempoApiImpl**

- `create_txn_env`: 复用 `create_mainnet_txn_env()`
- `transact`: 用 `TempoEvm::new(...)` 替代 `create_main_evm_from_state()`
- `inspect_tx_commit`: 同上
- 验证 `TxSetter for TxEnv` 已存在（mainnet impl 有）
- 验证 `DebankErrorCode: From<HaltReason>` 和 `PreErrorCode: From<HaltReason>` trait bounds 满足

- [ ] **Step 3: Add MultiChainCfgEnv::Tempo variant + chain_id() arm**

- [ ] **Step 4: Add build.rs match arm**

- [ ] **Step 5: Add mod declarations**

- [ ] **Step 6: Verify compilation**

Run: `cargo check -p leafage-evm-rpc 2>&1 | tail -10`

- [ ] **Step 7: Commit**

```
feat(tempo): wire up TempoApiImpl and MultiChainCfgEnv::Tempo
```

---

### Task 7: Add CLI entry point

**Files:**
- Modify: `bin/leafage-evm/src/standalone.rs`

- [ ] **Step 1: Add "tempo" evm_type**

`build_chain_cfg_env()` 加:
```rust
"tempo" => {
    let mut chain_cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
    chain_cfg.disable_balance_check = true;
    chain_cfg.disable_eip3607 = true;
    chain_cfg.disable_block_gas_limit = true;
    chain_cfg.disable_base_fee = true;
    chain_cfg.chain_id = chain_id;
    chain_cfg.tx_gas_limit_cap = Some(gas_cap);
    Ok(MultiChainCfgEnv::Tempo(chain_cfg))
}
```

`evm_type` arg parser 加 `"tempo"`, `parse_chain_cfg` 加 `"tempo" => 4217`.

- [ ] **Step 2: Verify full workspace compilation**

Run: `cargo check --workspace 2>&1 | tail -10`

- [ ] **Step 3: Commit**

```
feat(tempo): add 'tempo' evm_type CLI entry point (chain_id=4217)
```

---

### Task 8: Integration test against dev environment

使用 blockchain-misc-x3 dev 节点（`blockchain/tempo:e13d513`, 端口 8566）对照验证。

**前提:** leafage-evm 已同步 Tempo state（通过 pipeline Kafka/S3）。如果 dev 环境没有 leafage 实例，需要先部署。

- [ ] **Step 1: 验证 TIP20 balanceOf**

对照: writer 节点 `eth_call` → `0x20C0...` + `balanceOf(addr)`。验证 leafage 返回相同结果。

- [ ] **Step 2: 验证 eth_multiCall (多笔 TIP20 调用)**

- [ ] **Step 3: 验证 simulateTransactions (TIP20 transfer)**

对照: writer 端 `pre_traceMany` 结果（traces + logs + gasUsed）。

- [ ] **Step 4: 验证 estimateGas**

对照: writer 端 `eth_estimateGas`。

- [ ] **Step 5: 验证 0xeeee native token 模拟**

leafage 已有 `eth_erc20_handle()` 处理 `0xEeee...` 地址。Tempo native balance = 0，确认一致。

- [ ] **Step 6: 验证非 TIP20 普通合约调用**

确认 Tempo 上的普通 Solidity 合约（如 Multicall3）通过标准 EVM 执行正常。

- [ ] **Step 7: Commit test scripts/results**

```
test(tempo): integration test results against dev environment
```

---

## Implementation Notes

### 预编译移植策略

Tempo 用 `#[contract]` proc macro 生成 storage layout。leafage 不引入 proc macro crate，移植宏展开后的代码：
1. 在 task_tempo 中 `cargo expand -p tempo-precompiles` 查看展开结果
2. 或读 `dispatch.rs` 中的 selector 路由 + `mod.rs` 中的业务逻辑
3. 每个预编译 struct 需要手动实现 storage field 定义（`StorageSlot<T>` / `StorageMapping<K, V>`）

### AccountKeychain 签名验证

AccountKeychain 涉及 P256/WebAuthn 签名验证。在 leafage eth_call 模式下：
- 如果 DeBankCore 不 simulate 涉及签名验证的操作 → stub 即可
- 如果需要 → 加 `p256`, `sha2` 依赖，完整移植验证逻辑
- 建议: 先 stub，集成测试时确认是否命中

### ip_validation 模块

Tempo `lib.rs` 引用了 `ip_validation` 模块。检查是否有预编译使用它。如果只在测试中使用，可忽略。

### 代码量估算

| 组件 | Tempo 源码 | leafage 预估 |
|------|-----------|-------------|
| 预编译 (9个) | ~19,000 行 | ~15,000 行 |
| Storage 层 | ~4,700 行 | ~3,500 行 |
| Adaptation shim | — | ~300 行 |
| Hardfork + gas | ~80 行 | ~80 行 |
| TempoEvm | — | ~200 行 |
| TempoApiImpl | — | ~100 行 |
| CLI wiring | — | ~30 行 |
| **Total** | ~24,000 行 | ~19,000 行 |
