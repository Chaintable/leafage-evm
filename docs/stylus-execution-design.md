# leafage-evm Stylus/WASM 合约执行方案

状态：设计（未实现）
适用分支：`arb-evm`(Stylus 相关代码只在此线，不在 `main`)。本文行号锚点基于 review worktree `fix/arb-evm-opcode-env`（tip `b16d331`）。
参照实现：Chaintable/nitro fork（`~/ghorg/chaintable/nitro`，写节点）+ Chaintable/go-ethereum-arb（`debank` 分支，pipeline geth fork）。

---

## 0. 背景、目标与结论

**背景。** Arbitrum Stylus 允许用 Rust/C 编译成 WASM 部署合约。链上一个 Stylus 合约的 account code 是压缩后的 WASM blob，带 `0xEFF000`(classic) / `0xEFF001`(fragment) / `0xEFF002`(root) 前缀；部署后经 `ArbWasm.activateProgram` 预编译，把 `codeHash→moduleHash` 与激活元数据(init_cost / cached_cost / footprint / activatedAt / version)写进 ArbOS Programs 子空间。真正调用时，nitro 的 geth 在解释器里检测到 Stylus 前缀，把执行转交给 WASM 运行时，而不是跑 EVM opcode。

**现状一句话。** leafage-evm 目前**只实现了"激活 + 元数据"，没有任何执行路径**。一笔 tx CALL 到 Stylus 合约时，stock revm 拿到 `0xEF..` 前缀的 code，首字节 `0xEF` 是未定义 opcode → 立即 invalid-opcode halt、耗尽全部 forwarded gas、返回空 data、CALL 失败。相对 nitro 是**静默错误执行**：任何模拟(eth_call / debug_trace* / eth_multiCall / pre_traceMany / simulateTransactions)只要路径经过 Stylus 合约，结果就与 writer 不一致。

**目标。** 在 leafage 的单-tx 模拟里正确执行 Stylus 合约，使 gasUsed / 返回值 / 子调用 / 日志 与 nitro writer 的 `debug_traceCall` 对齐。

**结论先行(两个 headline 问题)：**

1. **现有数据能支撑执行 —— 能。** 执行的全部共识输入都在 leafage 已复制的状态里(链上 WASM、`codeHash→moduleHash`、StylusParams、arbos version、激活元数据)。编译后的 native 产物**不在共识里**，是从链上 WASM 确定性重算的节点本地派生物——nitro 自己也这么做。
2. **不需要新增持久化/pipeline 字段 —— 不需要。** 不改 pipeline diff(`codes/accounts/storages`)、不加 state trie 列、writer 也不用额外 emit module。唯一"净新增"是**可选的本地 native-asm 缓存**(性能，可后置)。
3. 一个真 caveat(不属于数据/字段问题)：`RecentWasms` 的跨-tx 块内 gas 语义会让"块中间某笔 Stylus tx"的 gas 精确复现需要按序重放整块——见 §3.4 与 §4.7。

**工作量定性。** 缺的是执行**机制**，不是数据：FFI 扩展、hostio 桥、call dispatch、ink/page 计量、native-asm 编译产物。主体难度在 hostio 桥(复刻一整套 EvmApi 语义)与 revm frame 接入。

---

## 1. Stylus 执行原理(nitro 参照)

### 1.1 调用拦截点

nitro 的执行**不在 call 层分叉出独立 VM**，而是在解释器内 hook：`go-ethereum/core/vm/interpreter.go` 的 `EVMInterpreter.Run` 载入 code 后、进 opcode loop 前，判断 `evm.chainRules.IsArbitrum && <isStylusProgramPrefix>(contract.Code, arbosVersion)`，命中则调 `evm.ProcessingHook.ExecuteWASM(callContext, input, evm)`(`go-ethereum/core/vm/evm_arbitrum.go` 的 `TxProcessor` 接口)→ `arbos/programs/programs.go` 的 `Programs.CallProgram`(`programs.go:193`)。同一套 CALL/STATICCALL/DELEGATECALL 机制照常到达 Stylus，只是**换掉 code-body 的执行方式**。

创建侧：`go-ethereum/core/vm/evm.go` 对 `0xEF` 起始 code 的 EIP-3541 拒绝在 Arbitrum + Stylus 前缀下放行，所以 Stylus code 能作为 account code 落库。

### 1.2 CallProgram 数据流(`arbos/programs/programs.go:193-297`)

```
getWasm(statedb, address, params)                 // 链上 account code 解出压缩 WASM (programs.go:140/399)
moduleHash = p.moduleHashes.Get(codeHash)         // Programs 子空间 {2} (programs.go:56/155/217)
cached = program.cached || RecentWasms.Insert(..) // 链上 cached 标志 + 块内 LRU (programs.go:232-241)
localAsm = handleProgramPrepare(...)              // 取本地编译 native asm；缺则从 WASM 重编译 (programs.go:265)
evmData = {basefee, chainid, coinbase, blocknum, ...}
callProgram(address, moduleHash, localAsm, scope, evm, calldata, evmData, goParams, ...) // programs.go:288
```

### 1.3 三个 native FFI(Chaintable/nitro `crates/stylus/src/lib.rs`)

| 符号 | 签名要点 | 产物 | 用途 |
|---|---|---|---|
| `stylus_activate` | `(wasm, page_limit, stylus_version, arbos_version, debug, out, codehash, out module_hash, out stylus_data, gas)` (`lib.rs:111`) | **WAVM module** `module.into_bytes()` + `module.hash()` | 共识锚：算 moduleHash / stylus_data。**无 target 参数**。leafage 已绑 |
| `stylus_compile` | `(wasm, version, debug, target, cranelift, out)` (`lib.rs:160`) | **native asm**(Wasmer 序列化，per-target) | 可执行产物。leafage **未绑** |
| `stylus_call` | `(module, calldata, StylusConfig, NativeRequestHandler, EvmData, debug, out, gas, long_term_tag)` (`lib.rs:268`) | 执行结果 + ink_left | 跑合约。leafage **未绑** |
| `stylus_target_set` | `(name, desc, out, native)` (`lib.rs:214`) | 设进程级 target 描述符 | 供 compile 用 |

**关键点**：`stylus_call` 吃的是 `stylus_compile` 产出的 **native asm**，不是 `stylus_activate` 产出的 WAVM module——`stylus_call` 内注释 "module came from compile_user_wasm" + `NativeInstance::deserialize_cached(module, ...)`(`lib.rs:283`)。moduleHash(来自 activate/prover)与 native asm(来自 compile/wasmer)是两个不同产物：前者共识、后者执行。

### 1.4 hostio(EvmApi)——15 个 RequestType

Rust 侧所有 host I/O 经**单一** `RequestHandler func(req RequestType, input) (output, extra, gas)` 回调(`arbos/programs/api.go:25,30-46`)：

```
GetBytes32          // SLOAD
SetTrieSlots        // SSTORE(可多槽)
GetTransientBytes32 // TLOAD
SetTransientBytes32 // TSTORE
ContractCall / DelegateCall / StaticCall   // 子调用
Create1 / Create2                          // 部署
EmitLog             // LOG0-4
AccountBalance / AccountCode / AccountCodeHash
AddPages            // 内存增长(page 计费)
CaptureHostIO       // tracing 用
```

每一项都映射到 merkle state 读写、transient storage、递归子调用或临时 page 模型——**没有节点本地依赖**。

### 1.5 ink 计量 / 栈 / 内存

`StylusConfig`(`crates/prover/src/programs/config.rs`)：`version: u16`、`max_depth: u32`(栈上限，字为单位)、`pricing: PricingParams`。`PricingParams.ink_price: u32`("bips of an evm gas")。换算线性：

```
gas_to_ink(gas) = gas * ink_price          // config.rs:87
ink_to_gas(ink) = ink / ink_price          // config.rs:91
```

`ink_price` / `max_depth` / page 参数都来自链上 **StylusParams**。内存按 64KB page，指数曲线计费(free_pages / page_gas / page_limit，`arbos/programs/memory.go`)；CallProgram 进入前先扣 memory init + program init/cached cost。

### 1.6 EvmData(传给 stylus_call 的上下文，`crates/arbutil/src/evm/mod.rs`)

```
arbos_version, block_basefee, chainid, block_coinbase, block_gas_limit,
block_number(=L1 块号), block_timestamp, contract_address, module_hash,
msg_sender, msg_value, tx_gas_price, tx_origin,
reentrant, return_data_len, cached, tracing
```

每一项都能从 revm 执行上下文 / block env / tx env 拿到，或从已复制状态派生(`module_hash` 来自 Programs；`block_number` 是 L1 块号，来自 ArbOS Blockhashes 状态，leafage 已有 `blockhashes_l1_block_number()`，`arbos_state.rs:216`)。

---

## 2. leafage 现状盘点

### 2.1 已实现(激活 / 元数据)

| 组件 | 位置 | 说明 |
|---|---|---|
| ArbWasm 预编译 `0x71` | `arbitrum/precompile/wasm.rs`(1492 行) | `activateProgram` + `codehashKeepalive` + ~18 getter，gated `arbos_version>=30` |
| ArbWasmCache 预编译 `0x72` | `arbitrum/precompile/wasm_cache.rs`(554 行) | 管理链上 cached 标志 + cache-manager 访问集，**不执行任何东西** |
| Programs 状态读写 | `arbitrum/precompile/state/stylus.rs`(488 行) | `stylus_params:26`、`wasm_program:238`、`active_wasm_program:257`、`wasm_program_age:283`、`save_wasm_module_hash:309`、`save_activated_wasm_program:322`、`wasm_program_cached:416` |
| WASM 解码/解压/dict | `wasm.rs:253-288,594-759`、`stylus_dictionary.rs` | 前缀常量 `wasm.rs:32-34`，brotli + vendored dictionary |
| `stylus_activate` FFI | `arbitrum/precompile/stylus_runtime.rs`(264 行) | **只绑 `stylus_activate` + `free_rust_bytes`**(`:123`)，经 `LEAFAGE_ARB_STYLUS_LIB` dlopen(`:9,89`) |
| 激活产物内存缓存 | `arbitrum/evm/context.rs:27,69-88` | `activated_wasm_modules: HashMap<B256,Bytes>` + `stylus_pages_open/ever` |

### 2.2 缺失(执行)

| 缺失项 | 应落点 |
|---|---|
| `stylus_compile` / `stylus_call` / `stylus_target_set` FFI 绑定 | `stylus_runtime.rs` |
| native-asm 缓存(现缓存的是 WAVM module，执行用不了) | `context.rs` / 新缓存层 |
| call dispatch hook(检测 callee code `0xEFF0` 前缀 → Stylus 分支) | `arbitrum/evm/mod.rs`(frame 路径 `:148-167`) |
| hostio / EvmApi 桥(15 个 RequestType → revm) | 新模块 |
| ink 计量 + page 计费接线(`stylus_pages_open` 现为只写、恒 0) | `context.rs` + 执行分支 |

### 2.3 现在 CALL Stylus 合约的实际行为

`ArbitrumEvm` 用 stock `EthFrame` + `EthInterpreter` + `EthInstructions`(`evm/mod.rs:34-42`)，`frame_init/frame_run` 直接透传(`:148-167`)，`instructions.rs` 只 override GASPRICE/BLOCKHASH，`handler.rs` 只 override fee/gas/refund——**没有 callee-code 前缀检查**。所以 `0xEF` 首字节 → invalid opcode → halt、耗 gas、空返回、CALL 失败。`activated_wasm_module()` getter 与 `stylus_pages_open` 系列目前**零消费者**(只写)。

---

## 3. 数据支撑判断(核心)

### 3.1 数据分层原理

nitro 的数据模型天然分层：**共识/merkle 状态**只存压缩 WASM(作为 account code) + Programs 元数据 + `codeHash→moduleHash` + StylusParams；真正的编译产物(native asm / wavm module)**不进共识**，存在节点本地独立 wasm store(`db.wasmdb`，按 `moduleHash+target` 键)，**由 activator 从链上 WASM 按需重算**(`arbos/programs/native.go:413-478` `getCompiledProgram`：先查内存/本地 store，miss 则 `getWasmFromContractCode` + `activateProgramInternal` 重编译，`:428-444`)。moduleHash 是共识锚，重编译后断言 `recomputed == stored` 否则报错(`native.go:448-451`)。native asm 是 per-target、per-node 派生物，target 选择(arm64/x86/cranelift)不影响共识。

历史重放确定性只依赖：**链上 WASM + program.version + 链上 StylusParams + arbos_version**(reactivation 用 merkle 里的 `program.version` 定版，`native.go:436-442`)。

### 3.2 执行输入逐项清单

| 执行输入 | 分类 | leafage 现状 |
|---|---|---|
| 压缩 WASM(account code) | 共识可复制 | ✅ `HashToCode` CF 内容寻址、verbatim、无 size cap(`leafage-evm-storage db.rs:155`) |
| `codeHash→moduleHash` | 共识可复制 | ✅ Programs `{2}` 已读(`state/stylus.rs:309`) |
| Programs 元数据 / StylusParams / activation gas | 共识可复制 | ✅ 已读(`state/stylus.rs`) |
| arbos version | 共识可复制 | ✅ `arbos_state.rs:190` |
| EvmData(basefee/chainid/coinbase/blocknum/timestamp/caller/value/origin/module_hash) | 上下文 + 共识派生 | ✅ revm 上下文 + `blockhashes_l1_block_number()`(`arbos_state.rs:216`) |
| 全套 hostio(SLOAD/SSTORE/子调用/create/log/balance/code…) | 共识 / 上下文 / 子调用 | ✅ 全部映射到 revm DB 读写、子调用、日志 |
| **编译后 native asm** | **节点本地可重算**(非共识、per-target) | ❌ leafage 无 wasm store —— **但从链上 WASM 确定性重编译即可，不是新数据源** |

**所有共识相关输入 leafage 都已复制且可点读**(ArbOS 账户存储无地址过滤全量复制，`arbos_state.rs` 已证明能按 keccak 预映像点读 Programs 子空间的确定性 slot)。唯一缺的编译产物是**从已有数据确定性重算的派生物**。

### 3.3 发射侧验证(风险① 已验证)

直接读 Chaintable/go-ethereum-arb(`debank` 分支)：

- `StateDB.StateDiff`(`core/state/statedb.go:1318`)对每个 `dirtyCode` 的 state object 执行 `codes[codeHash] = obj.code`(`:1353`)——**原样全量 code，无 size cap、无 Stylus 过滤**。
- `arbitrum/api_debank.go:137` 把 `StateDiff` 返回的 `codes` 一并打包(`GetOutPut(...codes)`)，整条路径**无 truncation**。
- Stylus 合约 code = `0xeff000..` WASM blob(普通 SetCode 部署)，从 geth 视角与任何 EVM 合约无异 → 自动完整带出；Programs 激活元数据是 ArbOS 账户 storage 变更，走 `storages` 一并发射。

**结论：发射侧不需要任何新增字段。** 残留仅通用问题——archive **bootstrap 快照**是否覆盖"leafage 起始点之前就已部署"的历史 Stylus code；这对所有合约 code 一视同仁，全量 state 快照导入即可覆盖，非 Stylus 特有。

### 3.4 唯一 caveat：RecentWasms 的块内 gas 语义

ArbOS ≥ 60，`CallProgram` 把 `recentWasmsCacheHit = statedb.GetRecentWasms().Insert(codeHash, params.BlockCacheSize)` 折进 `cached` 标志，而 `cached` 决定收 **cachedGas 还是 initGas —— 直接的 gas 差异**(`programs.go:232-241`)。这个 LRU 是**跨 tx、块内累积、块末丢弃**的临时状态(`go-ethereum/core/state/statedb_arbitrum.go:485-518`)。后果：**块中间某笔 Stylus tx 的 gas 精确复现，需要按序重放整块**(或至少同块在先的 Stylus 调用)；孤立单-tx 重放从空 RecentWasms 起步、gas 可能偏差。

这**不是**要维护的持久化字段(所以"不需要额外字段"成立)，而是执行器要维护的**运行期引擎状态**(连同 per-tx 的 `openWasmPages/everWasmPages` 页计数器，`statedb_arbitrum.go:366-367`)。首版策略见 §4.7。

### 3.5 结论

- **现有数据能否支撑执行：能(YES)。** archive 模式下还支持任意历史块重放。
- **是否需要额外维护字段：不需要(NO)。** 只需一个可选的本地派生 native-asm 缓存(性能)。

---

## 4. 实现方案

### 4.1 native lib 构建(cdylib)——前置

现状缺口：Chaintable/nitro fork 的 Makefile 只产 `libstylus.a`(**staticlib**，`Makefile:76/447`)，`crates/stylus/Cargo.toml` 是 `crate-type=["lib","staticlib"]`(`:48`)、**无 cdylib**；而 leafage 用 `libloading::Library::new`(dlopen)需要 `.so`/`.dylib`。

做法(择一)：
- 给 `crates/stylus` 加 `cdylib` crate-type，或
- 新建薄 wrapper crate re-export `stylus_activate/compile/call/target_set/free_rust_bytes`，构建成 `.so`。

版本 pin：`.so` 必须与目标链 writer 用的 nitro 版本同源构建(见 §6 风险②)，用不可变 tag。部署经 `LEAFAGE_ARB_STYLUS_LIB` 指向该 `.so`。

### 4.2 FFI 层扩展(`stylus_runtime.rs`)

- 新增 `StylusCompileFn` / `StylusCallFn` / `StylusTargetSetFn` 三个 extern C 类型 + `symbol()` 加载，签名照 `crates/stylus/src/lib.rs:160/214/268`。
- Rust 镜像结构：`StylusConfig{version,max_depth,pricing}`、`PricingParams{ink_price}`、`EvmData`(§1.6 全字段)、`NativeRequestHandler`(hostio 回调 + 上下文指针)、复用已有 `GoSliceData`/`RustBytes`/`Bytes32`。
- 进程级初始化：`stylus_target_set` 设 host arch target(如 `arm64`/`x86_64`)一次；`stylus_compile` 用它。
- 错误码映射沿用现有 `StylusRuntimeError`(OutOfInk=3 → `PrecompileError::OutOfGas` / OutOfStack=4 / NativeStackOverflow=5)。

### 4.3 native-asm 缓存

- 缓存键 `moduleHash`(共识锚)。首次 call miss → 从链上 WASM `stylus_compile(wasm, program.version, host_target, cranelift=false)` 得 native asm，回填。
- 改造 `context.rs` 的 `activated_wasm_modules`：目前存 WAVM module(执行用不了)，改成存/另存 native asm；或在其上加一层 `compiled_asm: HashMap<B256,Bytes>`。
- 可选落盘：非 state trie、非 pipeline 的独立本地 KV(按 `moduleHash+target`)，跨请求复用，纯性能，可后置。首版每次 call 现场 `stylus_compile` 也能跑，只是慢。

### 4.4 call dispatch hook

- 接入点：`arbitrum/evm/mod.rs` 的 frame 路径(`frame_init`/`frame_run`，`:148-167`)或 handler 的 frame 分发。在 callee code 载入后、进 revm 指令循环前，检测 `code.starts_with(0xEFF0..)` 前缀 → 走 Stylus 执行分支而非 `EthInterpreter`。
- 语义：CALL/STATICCALL/DELEGATECALL/CALLCODE 到 Stylus 合约都要命中(与 nitro 一致，前缀检查在 code-body 执行处，与 call 种类无关)。DELEGATECALL 下 `contract_address` / `msg_sender` / storage 上下文按 delegate 语义(acting address vs code address)。
- 这是与 revm 36.0 frame 抽象耦合最深、最需小心的改动；先确认 revm 36 的 frame trait 在哪能拿到 callee code + 注入自定义 body 执行。

### 4.5 hostio / EvmApi 桥(工作量最大)

实现单一 `handle_request(req: RequestType, input: &[u8]) -> (output, extra, gas_cost)`，逐项映射到 revm 的 `DatabaseRef` / `Journal` / 子调用：

| RequestType | 映射 | 注意 |
|---|---|---|
| GetBytes32 | SLOAD | 2929 warm/cold gas |
| SetTrieSlots | SSTORE(可多槽) | 2929 + refund 语义 |
| Get/SetTransientBytes32 | TLOAD/TSTORE | revm transient storage |
| ContractCall/Delegate/StaticCall | 递归子调用 | 63/64 规则 + call stipend + value 转账 + depth |
| Create1/Create2 | 部署 | init code、地址派生、collision |
| EmitLog | LOG0-4 | topics + data gas |
| AccountBalance/Code/CodeHash | 账户查询 | warm/cold；code 可能又是 Stylus(递归) |
| AddPages | 内存增长 | page 计费，接 `stylus_pages_open` |
| CaptureHostIO | tracing | 接 inspector；非 trace 模式 no-op |

这是复刻 nitro `arbos/programs/api.go:63-478` 一整套语义，最易在 gas / 2929 / 子调用边界出偏差。`reentrant` 计数、`return_data` 长度、readOnly 传播都要接对。

### 4.6 ink / gas / page 计量

- 进入前：`ink = gas_to_ink(gas_remaining)`(ink_price 来自链上 StylusParams)。
- 退出后：`gas = ink_to_gas(ink_left)`；`OutOfInk → OutOfGas`。
- 预扣：memory init cost + program init/cached cost(from Programs 元数据 / 激活的 `init_cost`/`cached_cost`/`footprint`)——`programs.go:227-262`。
- page 模型：接回 `context.rs` 的 `stylus_pages_open`(目前恒 0)，实现 AddPages 增长与指数计费。

### 4.7 cached 标志与 RecentWasms(caveat 落地)

首版策略(择一)：
- **A(简单，接受微小偏差)**：`cached` 只取链上 `program.cached`，忽略块内 RecentWasms。块中间 tx 的 Stylus init gas 可能与 writer 差一档(cachedGas vs initGas)。对 eth_call/估算影响小，trace 精确对账时需知晓。
- **B(精确，成本高)**：在整块重放上下文维护一个 RecentWasms LRU，按 tx 顺序 Insert，复现 writer 的 `cached`。仅在需要 bit-精确 gas 的场景启用。

`stylus_pages_open/ever` 页计数器每 tx 重置，按 tx 维护即可(non-caveat)。

---

## 5. 分阶段实施计划

| Phase | 内容 | 验收 |
|---|---|---|
| 0 | cdylib 构建(§4.1) + **moduleHash 复现实测**(§6 风险②) | 目标链一个真实已激活程序，leafage 侧 `stylus_activate` 产出 `module_hash == 链上 {2} 里的 moduleHash` |
| 1 | FFI 绑定 `compile/call/target_set`(§4.2) + native-asm 缓存(§4.3) | 能对一段 WASM `compile` 出 asm 并 `call` 返回(mock hostio) |
| 2 | dispatch hook(§4.4) + 最小 hostio(GetBytes32/SetTrieSlots/return)(§4.5) | 跑通一个只读/纯存储的 Stylus 合约，返回值对齐 writer |
| 3 | 完整 hostio(子调用 / create / log / account)(§4.5) | 跨合约(Stylus↔Solidity 互调)、日志、余额查询对齐 |
| 4 | ink/page/gas 精确对齐(§4.6-4.7) + 与 writer `debug_traceCall` 逐 case 对账 | 一组真实历史 tx 的 gasUsed / trace 与 writer 一致(cached caveat 内) |

---

## 6. 风险与验证

| 风险 | 状态 | 缓解 / 验证 |
|---|---|---|
| ① writer 是否 emit 完整 Stylus WASM | **已验证：发射侧完整** | `StateDiff` codes 全量原样(`statedb.go:1353`)、`api_debank` 无 truncation。残留：archive bootstrap 快照覆盖历史 code(通用，全量快照即可) |
| ② moduleHash bit-for-bit 可复现 | **降级：同一 lib 天然一致** | leafage 用 nitro 同一份 `crates/stylus`；只需版本 pin 与 writer 一致。**Phase 0 一次性实测**断言 hash 相等 |
| ④ 单 target | **可控** | 执行只需一个 host native target(`stylus_target_set`+`stylus_compile`)，不需 validator 多-target 集/wavm 证明 target。构建缺口：需 cdylib(§4.1) |
| RecentWasms 块内 gas 语义 | **存在(非数据字段)** | §4.7 策略 A/B；trace 精确对账时按需整块重放 |
| hostio 语义精度 | **主要工程风险** | 逐 RequestType 对账 gas/2929/子调用边界(Phase 3-4) |
| revm 36 frame 耦合 | **需先验证** | Phase 1 先确认 frame 抽象能拿到 callee code 并注入自定义 body |

---

## 7. 附录：关键代码位置

**leafage-evm(`arb-evm` 线)：**
- `crates/leafage-evm-chains/src/arbitrum/precompile/wasm.rs`(ArbWasm 0x71，前缀 `:32-34`，activate `:253`)
- `.../precompile/wasm_cache.rs`(ArbWasmCache 0x72)
- `.../precompile/stylus_runtime.rs`(FFI，只绑 `stylus_activate` `:123`；env `:9`)
- `.../precompile/state/stylus.rs`(Programs 状态读写)
- `.../arbitrum/arbos_state.rs`(PROGRAMS_SUBSPACE `[8]`；`arbos_version:190`；`blockhashes_l1_block_number:216`)
- `.../arbitrum/evm/context.rs`(`activated_wasm_modules:27`；`stylus_pages_open:77-88`)
- `.../arbitrum/evm/mod.rs`(frame 路径 `:34-42,148-167`)
- `crates/leafage-evm-storage/`(`HashToCode` CF `db.rs:155`；archive；`BlockNumToBlockHash` `archive/mod.rs:1248`)

**nitro(Chaintable fork，写节点)：**
- `arbos/programs/programs.go`(`CallProgram:193`，`getWasm:140/399`，`moduleHashes {2}:56`，`localAsm:265`，`callProgram:288`，`RecentWasms:232-241`，expiry `:551-570`)
- `arbos/programs/native.go`(`activateProgram:193`，`activateProgramInternal:304`，`compileNative:359`，`getCompiledProgram:413-478`，moduleHash 断言 `:448-451`，`WasmTargets:415`)
- `arbos/programs/api.go`(RequestType 枚举 `:30-46`)
- `crates/stylus/src/lib.rs`(`stylus_activate:111`，`stylus_compile:160`，`stylus_target_set:214`，`stylus_call:268`，`stylus_cache_module:340`)
- `crates/stylus/Cargo.toml`(`crate-type:48`，无 cdylib)；`Makefile:76/447`(libstylus.a)
- `crates/prover/src/programs/config.rs`(`StylusConfig`/`PricingParams`，`gas_to_ink:87`/`ink_to_gas:91`)
- `crates/arbutil/src/evm/mod.rs`(`EvmData` 结构)
- `go-ethereum/core/vm/interpreter.go` + `evm_arbitrum.go`(Stylus 前缀 hook → `TxProcessor.ExecuteWASM`)

**go-ethereum-arb(`debank` 分支，pipeline)：**
- `core/state/statedb.go`(`StateDiff:1318`，`codes[codeHash]=obj.code:1353`)
- `arbitrum/api_debank.go`(`StateDiff` 调用 `:137`，`GetOutPut(...codes):148`)

---

## 8. 测试方案

### 8.0 两个目标与既有测试设施

**目标 A —— 测到新功能**：证明 Stylus 执行结果(return / gasUsed / 子调用 / 日志 / storage 变更)与 nitro writer 一致。
**目标 B —— 不影响原有**：dispatch hook 动了**所有合约共享**的 frame 执行路径，是回归风险核心，必须证明非 Stylus 合约的行为逐字节不变。

**权威 oracle**：nitro writer 同块 `debug_traceCall` / `eth_call`(已有 hood writer，nitro v3.11.1)/ Arb One 官方 RPC。

**项目已有的三层测试设施(方案直接复用，不自造)：**

| 设施 | 位置 / 能力 | 用途 |
|---|---|---|
| in-crate 单测 | 各模块 `#[cfg(test)]`，`CacheDB<EmptyDB>` + `insert_account_storage`(见 `instructions.rs`/`arbos_state.rs`/`handler.rs`) | 组件级正确性 |
| leafage-bench 语料回放 | `bin/leafage-bench`，corpus 为生产真实 `eth_call`(按 L1/L2/L3 复杂度分类，`corpus.rs:57 CorpusCase`)；`--compare-url`(perf 对比 QPS/latency/error%)、`--output-dir`(dump 每 case 输出，`bench.rs:21`) | 回归差分 + 性能回归 |
| consistency checker | `rpc_verification/`(`CheckHistoryBlock`/`CheckHistoryState` vs official，输出 pass/fail summary) | 块/状态级一致性 |
| node-rpc-testing skill | 自定义 RPC(`eth_multiCall`/`pre_traceMany`/`trace_transaction`)逐字段 vs official 的方法论 | 自定义 RPC 一致性 |

**分层策略**：单测 → 组件(真 libstylus + fixture) → 与 writer 差分 → 回归(corpus 输出差分 + consistency + perf)。

### 8.1 目标 A：测新 Stylus 功能

**8.1.1 单元测试(in-crate，mock state)**
- dispatch 检测：`0xEFF000/01/02` 前缀 + arbos 版本 gate 的命中/不命中/边界。
- FFI roundtrip：`stylus_compile → stylus_call` 一段最小 fixture WASM(需真 libstylus，见 8.4)。
- hostio 逐项映射：GetBytes32 / SetTrieSlots / Get·SetTransient / Contract·Delegate·StaticCall / Create1·2 / EmitLog / AccountBalance·Code·CodeHash / AddPages —— 每项断言对 revm state/journal 的读写与 gas。
- ink / page：`gas_to_ink`·`ink_to_gas` 边界、`OutOfInk→OutOfGas`、page 指数计费、`stylus_pages_open` 接线。
- **moduleHash 复现(Phase 0 gate)**：fixture WASM `stylus_activate` → 断言 `module_hash` 稳定、且等于已知向量。
- 沿用现有风格：`CacheDB<EmptyDB>` 构造 ArbOS 账户存储 + 合约 code + Programs 元数据。

**8.1.2 组件 / fixture 测试(真 libstylus + 已知合约)**
- fixture 来源：nitro `crates/stylus/tests/` 测试 WASM、stylus-sdk 示例(erc20 / counter / keccak)编译产物。
- 端到端：构造含 `0xEFF0` code + Programs 状态的 `CacheDB`，CALL，断言 return / gasUsed。
- hostio 矩阵：纯计算 / 存储读写 / 跨合约(Stylus→Solidity、Solidity→Stylus 互调) / log / create / 余额查询 / 重入。

**8.1.3 与 writer 差分(gold standard)**
- 找目标链真实 Stylus 合约：扫链上 account code 前缀 `0xEFF0` 的账户，或已知 Stylus dApp。
- 抓这些合约的真实历史调用(见 8.3)。
- 方法(node-rpc-testing skill)：同块对 leafage 与 writer 分别调，逐项对比——
  - `eth_call` return bytes 相等；
  - `debug_traceCall` gasUsed / 子调用树 / logs / storage 变更 相等。
- 覆盖维度：不同 arbos version、cached / 非 cached(见 §3.4 caveat)、各 hostio 组合。

### 8.2 目标 B：回归(不影响原有)

**8.2.1 核心论点 + 直接证明：dispatch gate 隔离**
- gate = `0xEFF000/01/02` 前缀 + arbos 版本判断，**须与 nitro `IsStylusProgramPrefix` 精确条件逐字对齐**(3 种前缀 + 版本)。非 Stylus code 永不进 Stylus 分支。
- EIP-3541(London 起)禁止 `0xEF` 起始 code 的创建，Arbitrum nitro genesis 全在 London 后 → 链上唯一 `0xEFF0` code 就是 Stylus，gate 无误判(**无假阳性**)。
- 单测：普通合约(非前缀 code)断言不 dispatch 到 Stylus；gate 是廉价前缀比较、无副作用。
- 反向风险：gate 条件若与 nitro 不一致(多/少一种前缀、版本判断错)= 误 dispatch，**既是回归也是正确性 bug**，单测须钉死条件。

**8.2.2 全量单测绿**
- `cargo test -p leafage-evm-chains -p leafage-evm-rpc`(现有 279+36)零回归。

**8.2.3 corpus 输出差分(关键回归 gate)**
- leafage-bench `--output-dir` dump 每 case 输出；分别跑**改动前**(base `arb-evm`)与**改动后**(stylus 分支)同一 corpus。
- 断言两次 dump 的 per-case 输出 bytes **逐字节相等**——corpus 绝大多数是非 Stylus 合约，byte-identical 即证明既有路径零变化。
- 注：bench `--compare-url` 本身比的是 QPS/latency/error%(perf)，**正确性差分靠 output dump 自行 byte-diff**(或给 bench 补一个 `--assert-outputs-match` 开关，一次性小改)。error% 的 delta 已能捕获粗粒度正确性回归(某 case 从成功变报错或反之)。

**8.2.4 consistency checker(rpc_verification)**
- 历史非 Stylus 块跑 `CheckHistoryBlock`/`CheckHistoryState` vs official，一致率不回归。

**8.2.5 自定义 RPC 回归(node-rpc-testing)**
- `eth_multiCall` / `pre_traceMany` / `trace_transaction` 对非 Stylus tx 结果不变。

**8.2.6 性能回归**
- leafage-bench 对全 corpus 的 QPS/latency 改动前后在噪声内(前缀检查开销可忽略，用 bench 证明)。

**8.2.7 未配置 libstylus 的降级安全**
- 不设 `LEAFAGE_ARB_STYLUS_LIB` 时：非 Stylus 执行**完全正常**；CALL Stylus 合约返回清晰错误(`Unconfigured`)、**不 panic**、不污染其他执行。
- 测：env 缺失下全 corpus + 全单测通过。

### 8.3 测试数据来源(已落实具体实例)

**A. 受控 fixture —— nitro `crates/stylus/tests/`(已确认存在)**：约 15 个 Rust Stylus SDK 合约 + 一批手写 `.wat`，正是 nitro 自己 system_tests 用的(`rustFile("multicall")` 等)。按 hostio 覆盖映射：

| fixture | 覆盖的 hostio / 能力 |
|---|---|
| `keccak` / `keccak-100` / `math` / `add.wat` / `clz.wat` | 纯计算(最小 hostio)，先跑通 compile→call |
| `storage` / `sdk-storage` | GetBytes32 / SetTrieSlots(SLOAD/SSTORE) |
| `multicall` | ContractCall / DelegateCall / StaticCall(跨合约) |
| `evm-data` | EvmData 全字段(basefee/coinbase/blocknum/msg_sender/…) |
| `log` | EmitLog(LOG0-4) |
| `create` | Create1 / Create2 |
| `read-return-data` / `return-size.wat` / `write-result-len.wat` | 返回数据处理 |
| `memory*.wat` / `grow` / `pay-for-memory-grow.wat` | AddPages / page 计费 |
| `fallible` / `exit-early` | 错误路径(revert / OutOfInk) |
| `hostio-test` | 广覆盖 hostio |
| `erc20` | 真实完整合约(端到端) |

`.wat` 可直接用作单测输入；Rust 目录需 cargo-stylus 编译成 WASM。

**B. 真实链上 Stylus 合约(已验证一例)**：Arbitrum One `0xe6fc94f78cfec8bdf090ccb854e9b4382870aa7e`，code 前缀 `0xeff00000…`、15,820 字节压缩 WASM(`eth_getCode` 已确认)。

定位方法(确定性、无需 explorer API key)：ArbWasmCache 预编译 `0x0000…0072` 在缓存时 emit `UpdateProgramCache(address,bytes32,bool)`(topic0 `0x1bfaa29b…`)。
```
eth_getLogs(address=0x…0072, topic0=0x1bfaa29b…, 近 N 万块)
  → 取 log 的 transactionHash → eth_getTransactionByHash → cacheProgram 入参即程序地址
  → eth_getCode(地址) 断言前缀 0xEFF0；log 的 topic2 是该 code 的 codehash 可交叉核对
```
(Arb One 上 `cacheProgram` 频率低，20000 块内常为 0，需 ~千万块范围;`activateProgram` 到 `0x71` 更频繁但无事件，需扫块 tx。)

**目标链 hood 现状(已验证：当前无 Stylus 合约)**：hood = Robinhood testnet，chainId `0xb626`(46630)，official RPC `https://rpc.testnet.chain.robinhood.com`，blockscout `https://robinhoodchain.blockscout.com`，ArbOS 61(协议层支持 Stylus)。判定方法与证据：
- **决定性**：每个可调用的 Stylus 合约都必须经 ArbWasm 预编译 `0x71.activateProgram` 激活。hood 的 `0x71` 全时段**直接 tx=0、gas=0、internal tx=0**;对照组 ArbSys `0x64` 有 332 直接 + 50 internal(证明 blockscout 确实索引预编译调用，非盲区)→ `0x71` 的零是真零。
- 佐证：`UpdateProgramCache`(0x72)全历史 getLogs = 0(无缓存程序);blockscout 已验证合约按 `stylus_rust` 过滤 = 0。
- **含义**：为 hood 实现 Stylus 执行**当前不是必需**(没有 Stylus 合约可执行)。该功能的价值在于 ① 若 Robinhood 之后部署 Stylus;② leafage arb 代码复用到 Arbitrum One / 其它有 Stylus 的 Orbit 链。**这是 unverified 状态,会随时间变化——上线前重跑此检查。**
- Arb One 的实例(§8.3 B)可用于对**官方 RPC** 做差分、以及作为 code 形态样本。

**C. corpus**：现有 ingest 抓生产 `eth_call`。**注意**：当前 Stylus call 在 leafage 会失败，可能从未被 ingest 进 corpus，需**专门补采** Stylus 合约调用样本(从 writer 侧成功的调用抓)。

**D. oracle**：hood writer(nitro v3.11.1)、Arb One 官方 RPC(`https://arb1.arbitrum.io/rpc`，chainId `0xa4b1`，已验证可达)。

### 8.4 CI 与 gate

- 单测(mock 部分)：现有 CI，无需 libstylus。
- 组件 / fixture / 执行测试：CI 镜像需带 `libstylus.so`(与 writer **同版本 pin**，见 §4.1、§6 风险②)。
- **Phase 0 gate**：moduleHash 复现实测通过，才进入后续实现。
- **回归 gate(merge 前置)**：corpus 输出差分 byte-identical + 全单测绿。
- **差分 gate(nightly)**：对 writer 跑 Stylus corpus(正确性)+ 非 Stylus 一致率(回归)。

### 8.5 测试项与实施 Phase 对应

| Phase(§5) | 主要测试项 |
|---|---|
| 0 cdylib + moduleHash | 8.1.1(moduleHash 复现)、8.4 Phase 0 gate |
| 1 FFI + 缓存 | 8.1.1(FFI roundtrip)、8.2.7(降级安全) |
| 2 dispatch + 最小 hostio | 8.1.2(只读/存储 fixture)、8.2.1-8.2.3(dispatch 隔离 + corpus 差分) |
| 3 完整 hostio | 8.1.2(跨合约/log/create)、8.1.3(与 writer 差分) |
| 4 ink/page/gas 对齐 | 8.1.3(gasUsed 逐 case 对账)、8.2.4-8.2.6(consistency + 自定义 RPC + perf) |
