# leafage-evm Stylus 执行实现 —— 实施记录

基线：PR #184 (`fix/arb-evm-opcode-env`)。开发分支：`feat/stylus-execution`（worktree `leafage-evm.worktrees/stylus-exec`）。
配套：`docs/stylus-execution-design.md`（设计）+ `docs/stylus-execution-impl-plan.md`（实现计划，Phase 0-4）。

## 环境 / 构建配方（复用）

- **nitro cdylib worktree**：`nitro.worktrees/stylus-cdylib`，分支 `feat/stylus-cdylib`，base commit `8c6468aa2`（= 最新不可变 tag `v3.11.1-debank-2` + 2 commits）。
- **dylib 构建配方**（macOS arm64 dev）：
  1. `git submodule update --init --recursive crates/tools/wasmer brotli`
  2. `./scripts/build-brotli.sh -l`（产 `target/lib/libbrotli{common,enc,dec}-static.a`；需 cmake）
  3. `~/.cargo/bin/cargo build --release --lib -p stylus`（**必须用 rustup shim**，否则 PATH 里 Homebrew cargo 1.96.1 会忽略 `rust-toolchain.toml` 的 1.93.0 pin）
  - 产物：`nitro.worktrees/stylus-cdylib/target/release/libstylus.dylib`（19.6 MB）
- **leafage 加载**：`LEAFAGE_ARB_STYLUS_LIB=<abs>/libstylus.dylib`（绝对路径绕开 rpath）。

## Phase 0 —— cdylib + moduleHash 复现

- `[2026-07-15][done] cdylib crate-type` — nitro `crates/stylus/Cargo.toml` 的 `[lib] crate-type` 加 `cdylib`（Option A 一行，`prover_ffi` re-export 自动带 `free_rust_bytes` 进 dylib）。
  **Done:** `libstylus.dylib` 构建成功，`nm -gU` 导出全部符号：`stylus_activate/compile/call/target_set/cache_module/evict_module` + `free_rust_bytes`。

- `[2026-07-15][decided] 构建前置坑` — 三个必踩坑已记入构建配方。
  **Decision:** ① rustup shim 强制 1.93.0；② brotli 需先 `build-brotli.sh -l` 出 C 静态库（否则 `brotlienc-static not found`）；③ 本机装了 cmake（Homebrew，用户自有 mac，标准 dev 依赖）。

- `[2026-07-15][done] moduleHash 复现 fixture` — Arb One `0xe6fc94f78cfec8bdf090ccb854e9b4382870aa7e`（classic `0xeff000` 前缀，15,820 字节压缩 WASM，Stylus version 2）。
  **Done:** 链上 ground truth（Arb One RPC，ArbOS 51）：
  - `code_hash = 0x81fed44646b50a25748f80764d1d2f4d3fbbcc49300eb9b52ab197173334e024`
  - `module_hash = 0xa7c2ce01cea0880198cfc8a35bb3b772babc7ab007a8ebf4f9df1e35f8c6b098`（Programs `{2}` 子空间点读）
  - program meta：version=2, initCost=7843, cachedCost=3077, footprint=17, cached=true
  - fixture 存 `crates/.../precompile/fixtures/arb1_stylus_code.hex`，测试 `wasm.rs::tests::reproduces_arb_one_module_hash`（gated on `LEAFAGE_ARB_STYLUS_LIB`）。

- `[2026-07-15][done] moduleHash 复现实测 —— Phase 0 门通过` — `arbos_version=0` 直接复现成功，无需 fallback 到 51。
  **Done:** `cargo test -p leafage-evm-chains reproduces_arb_one_module_hash`（`LEAFAGE_ARB_STYLUS_LIB=<dylib>`）通过：leafage `stylus_activate`（经真 dylib）+ classic 解码，对 version-2 WASM 产出 `module_hash == 0xa7c2ce01…`，== 链上 Programs `{2}`。证明 dylib + FFI ABI（StylusActivateFn/StylusData/GoSliceData/RustBytes）+ 解码全部逐字节复现共识。确认 hash 只依赖 (wasm, stylus_version)，与 arbos_version 无关。

- `[2026-07-15][open] 写节点 nitro commit 核实` — dylib pin 在 `8c6468aa2`；需 SRE/镜像侧确认写节点实跑 commit == 此值（现 INFERRED）。moduleHash 复现只依赖 (wasm, version)，与 arbos 无关，故本 fixture 的复现不受写节点 commit 影响；但 prod 上线的 dylib 必须与写节点同 commit。

## Phase 1 —— compile FFI

- `[2026-07-15][done] stylus_compile 绑定` — `StylusRuntime::compile_from_env`（空 target = native host，cranelift=false/singlepass）。
  **Done:** `compiles_arb_one_program_to_native_asm` 通过：fixture WASM → 非空 native asm。全量单测 284 绿（env 未设时 Stylus 测试 skip = 降级安全）。

## Phase 2 —— call FFI + frame seam + hostio

- `[2026-07-15][done] Phase 2a: stylus_call FFI + hostio 桥 ABI` — `stylus_runtime.rs` 加：`EvmData`/`StylusConfig`/`PricingParams`/`NativeRequestHandler`/`RustSlice`/`Bytes20` 镜像结构（逐字段匹配 nitro，repr(C) 不 pack）、`StylusCallFn`、`HostioHandler` trait（语义边界，DB 泛型不进 FFI）、`HostioBridge` + `hostio_trampoline`（arena 保活 raw_data）、`StylusExecInput`/`StylusOutcome`/`StylusCallResult`、`call_from_env`。
  **Done:** `calls_arb_one_program_via_ffi` 通过：真 Arb One 程序经 leafage call 路径**端到端执行**（无 segfault = repr(C) 布局全对），gas 消耗，clean outcome。即整个 call FFI ABI 已验证。
  **Note:** 这些 pub(crate) 项在 non-test build 暂为 dead_code（21 warning），待 Phase 2c frame seam 消费后消除；仓库无 deny-warnings，不阻塞 CI。

- `[2026-07-15][done] Phase 2b: native-asm 缓存 + wasm_module_hash reader` — `context.rs` 加 `compiled_asm: HashMap<B256,Bytes>` keyed by module_hash（单 host 只 native target）+ 访问器；`state/stylus.rs` 加 `wasm_module_hash(code_hash)->B256` reader。均被 frame seam 消费。

- `[2026-07-15][done] Phase 2c: frame seam（frame_run）` — `evm/stylus.rs` 新模块 + `evm/mod.rs` override `frame_run`：检测 `0xEFF0xx` 前缀 → `run_stylus_frame`（gather 帧输入 → `ArbWasm::prepare_stylus_program` 读 Programs 状态+解码 → compile/缓存 → 预扣 init/cached gas → 组装 EvmData → `call_from_env` → 建 `InterpreterResult` → 走 stock `process_next_action`）。非 Stylus 走原 `self.inner.frame_run()`。借用 split（frame_stack vs ctx 两个 disjoint 字段）已验证编译。
  **Done:** 编译干净（dead_code 从 21→0，FFI 全被消费），全量 287 单测绿（含新 dispatch gate 2 测），无回归；dylib FFI 三测（moduleHash/compile/call）仍绿。
  **未做（Phase 3/4）：** 端到端 frame-path 执行集成测试（需 full-transact harness + 播种 Programs 状态）—— 是 Phase 2 真正验收门 + Arb One eth_call 差分起点。

- `[2026-07-15][done] Phase 2c+: inspect_frame_run override（traced 路径）` — `evm/mod.rs` 的 `InspectorEvmTr` impl 加 `inspect_frame_run`：检测 `0xEFF0` → `run_stylus_frame` + fire inspector `call_end`（`revm::inspector::handler::frame_end`，用 `ctx_inspector_frame()` 取 ctx/inspector/frame）；非 Stylus 委托 `self.inner.inspect_frame_run()`。闭合 PR #184 B2 同类缺口（traced RPC 不再把 Stylus 当坏 EVM 跑）。
  **Done:** 编译干净，287 单测绿。`call` hook 已在 inspect_frame_init 触发，此处补 `call_end` 防 inspector 调用栈错乱。

- `[2026-07-15][done] Phase 2d: 最小真 hostio` — `StylusHostio` 实现 `HostioHandler`：GetBytes32(SLOAD)/SetTrieSlots(SSTORE)/GetTransient(TLOAD)/SetTransient(TSTORE) 走 revm journal，wire 逐字节消费，static 保护。其余 req（4-14 calls/create/log/account/pages/capture）返回安全空默认（TODO Phase 3）。
  **未做（Phase 4 gas 对齐）：** SLOAD/SSTORE gas 为近似 EIP-2929/2200（非 nitro `Wasm*Cost` 精确），refund 未从 SStoreResult 累加（现恒 0），memory 页模型/RecentWasms 未接，EvmData.block_number 用 L2（应 L1）、tx_gas_price 用 raw（应 paid price）。全部标 `TODO(Phase 4)`，须对 writer 差分校准。

- `[2026-07-15][done] Phase 3: 完整 hostio` — `StylusHostio` 补齐：storage(0-3)、subcalls(4-6 Call/Delegate/Static 经 `drive_subframe` 同步驱动子帧,直接子帧手动 pop 不走 `frame_return_result`)、create(7-8 Create/Create2)、log(9)、account(10-12 balance/code/codehash)、pages(13)。仅 CaptureHostIO(14) 为 no-op(纯 tracing)+ reentrant flag 恒 0。
  **Done:** 全部编译干净,287 单测绿,dylib call 测绿,clippy 干净。`StylusHostio` 持 `&mut ArbitrumEvm` 以驱动子帧。
  **未验证(CRITICAL):** subcall/create 的子帧驱动 —— 正确性(子结果/gas/status/address)与 frame-stack 裸指针·借用**soundness** —— 在本环境**无法执行验证**(无跨合约 Stylus 调用),必须跑真实跨合约调用 + 对 writer 差分。gas 全为近似(见下 Phase 4)。

### 验证前沿（CRITICAL —— 必须诚实标注）

Phase 2c 起的执行引擎是**共识级 gas/trace 代码，其正确性只能对着活的 Arbitrum writer 或 Arb One traced RPC 差分验证**（设计 §8.1.3 gold standard）。当前环境：
- **hood 目标链无 Stylus 合约**（设计 §8.3 已验证），无法本地端到端测。
- **Arb One 官方 RPC 可作 oracle**（`eth_call`/`debug_traceCall` at pinned block），但需先把 frame seam + hostio 建到能跑真实合约、并 fetch 该合约读的 storage slots。

**结论**：FFI 基础层（activate/compile/call）已建成并对着 Arb One 共识数据验证。执行引擎（frame seam + hostio + gas）是下一里程碑，**必须以 Arb One eth_call 差分作为验收测试来建**，不能盲写共识代码当"完成"。已验证 = 会编译 + FFI ABI 对；未验证 = gas/trace 与 writer 是否逐 case 一致。

## 跨 Phase 待定（迁自实现计划 §9）

- `[2026-07-15][open] RecentWasms 策略 A/B` — 首版 A（忽略块内 LRU），Phase 4 视 writer 对账偏差决定是否上 B。
- `[2026-07-15][open] native-asm 落盘缓存` — 首版每次 call 现场 `stylus_compile`；落盘后置。
- `[2026-07-15][decided] 范围` — 用户定：全量 Phase 0-4（为 Robinhood 后续 + Arb One/Orbit 复用）。hood 当前无 Stylus 合约（设计 §8.3 已验证），上线前重跑该检查。

## 2026-07-21 对着 nitro 复核 hostio 语义

- `[2026-07-21][done] 修 3 个 hostio response 编码 bug（确定性错误，无需 oracle）` — commit `0369975`。三处 response 字节写错，Rust runtime 把它们当控制流信号读，后果是程序中止/继续的差别，不是 gas 精度。
  **Done:** ① `set_transient` 所有路径返回空 → runtime 读不到 status 字节，`req.rs:174` 报 `empty result!` 直接中止程序，**任何用 transient storage 的 Stylus 合约必然全盘失败**；改为 Success(0)/WriteProtection(3)/Failure(1)。② `create` 把子调用失败也返回 `0x00`-前缀（= 错误串，`req.rs:88-96` 中止程序），而 nitro 失败时返回 `0x01 ‖ 零地址` 让程序继续（`api.go:250`，与 EVM CREATE 压 0 一致）；`0x00` 现在只留给 nitro 同样拒绝的 static/畸形请求，且按 `api.go:425` 烧掉整个请求 gas；return data 只在 revert 时保留（`api.go:257-258`）。③ `emit_log` 用相反约定——空 = 成功、非空 = 错误串（`req.rs:266`），static 时返回空等于静默放行；改为返回 geth 的 `write protection`。3 个新单测（291 全绿），已反向验证：把三处改回原样，3 个测试全部 FAILED。

- `[2026-07-21][decided] 推翻两条既有判断——照原 TODO 改反而会引入 bug` — 复核 nitro 源码后确认。
  **Decision:** ① **call 的 status 字节不是 apiStatus**。`apiStatus`（Success0/Failure1/OutOfGas2/WriteProtection3）只用于 SetTrieSlots(1) 和 SetTransientBytes32(3)；ContractCall/Delegate/Static(4/5/6) 的字节是 `UserOutcomeKind`，Go 侧只产 0 和 2（`api.go:409-411`），**leafage 现有的 0/2 是对的**，不要"补齐四态"。create(7/8) 是第三套编码（成功标志位）。三套互不通用。② **`stylus.rs:217` 的 TODO "应为 paid gas price (GasPriceOp)" 本身是错的**：`programs.go:279` 明确用 `evm.TxContext.GasPrice`（raw effective price）；`GasPriceOp`/`GetPaidGasPrice` 只服务 EVM 的 GASPRICE opcode，nitro 自己这两条路径就不一致。该 TODO 待删，真正要核的是 revm `tx().gas_price()` 是否等于 geth 的 effective price 而非 max_fee_per_gas（**未验证**）。

- `[2026-07-21][open] setTrieSlots 的 gasLeft 检查 + OutOfGas 状态没做，移到 gas 对齐组` — 本轮只做 wire 编码，这条做不了纯编码修复。
  **Decision:** nitro 逐 slot 在**写入前**比对请求头的 gasLeft，超了就把 gasLeft 归零、跳过该 slot 并返回 OutOfGas(2)（ArbOS≥50；hood=61）；"恰好用光"也算 OOG（`api.go:82-118`）。leafage 现在跳过开头 8 字节的 gasLeft 从不检查，永远返回 Success。要对齐必须在 `journal.sstore` 提交前算出 SSTORE cost，而 revm 的 cost 依赖 sstore 返回的 `SStoreResult`（original/present/new + is_cold）——先 sload 会把 slot 变 warm、算错 cost。属于 gas 语义改动，与 subcall/create base cost 一起做。

- `[2026-07-21][done] AccountCode(11) 少收 700 gas（原 TODO 未列）` — 记录待修，本轮未动。
  **Decision:** nitro 用 `WasmAccountTouchCost(withCode=true)` = `(MaxCodeSize/24576)*ExtcodeSizeGasEIP150(700)` + 2929 access，标准链 cold **3300** / warm **800**（`operations_acl_arbitrum.go:157`）。leafage 用的是不带 code 的 2600/100，少 700；且缺 `gas < cost` 时返回空 code + 全额 cost 的路径（Rust 侧靠它触发 OOG，`api.go:289-299`）。AccountBalance(10)/CodeHash(12) 是 withCode=false，leafage 的 2600/100 正确。

- `[2026-07-21][decided] Arb One 不能作为 gas 对账 oracle` — hood 是 **ArbOS 61**（block 64715 从 51 升级，见 task_robinhood `docs/todo.md`），Arb One fixture 是 **ArbOS 51**。
  **Decision:** 共识 page limit（ArbOS≥59，`StylusParams.PageLimit` 默认 128）和 RecentWasms（ArbOS≥60，命中省 `initGas`≈8832+init_cost，典型 ~14.5k gas / 8.4×）**在 Arb One 上根本不触发**，用它差分测不出来，而这两条在 hood 上真实生效。Arb One 只能验 FFI/moduleHash/基础 hostio；共识 gas 对账必须拿 hood writer 做 oracle。**RecentWasms 策略 A 在 hood 上是错的，不是"差一档"。**

- `[2026-07-21][done] reentrant flag 落地` — commit `b9bca68`。原来恒 0，等于让 stylus-sdk 默认 entrypoint 的重入保护完全失效。
  **Decision:** 不侵入 revm 的 frame 钩子。nitro 的 `TxProcessor.Programs` 计所有非 delegate 帧，但**只查询 acting program 自己的地址**，而一个地址只有一份代码——所以"该地址的 Stylus 帧数"与之等价。实现放在 `run_stylus_frame` 自己的进入/退出点（`ArbitrumExecutionContext.open_stylus_frames`），DELEGATECALL/CALLCODE 不开 span（读 `frame.input` 的 scheme 判断）。放弃了在 `frame_init`/`frame_return_result` 配对的方案：revm 对 precompile/空代码返回 `ItemOrResult::Result` 不压帧，却**仍然会走 `frame_return_result`**，在那里递减会误伤父帧。

- `[2026-07-21][done] tx_gas_price 改用 effective price` — commit `b9bca68`。承接上面推翻的第②条。
  **Done:** revm 的 `gas_price()` 对 1559 tx 返回 **max_fee**（其文档明说），而 geth 的 `TxContext.GasPrice` 是 effective price `min(maxFee, baseFee+tip)`。改用 `tx().effective_gas_price(block.basefee())`（revm 语义与 geth 一致，已核对实现）。原 TODO 指向的 paid price 是错的方向。

- `[2026-07-21][done] hostio gas 对齐 5 项` — commit `d0053f8`。
  **Done:** ① subcall 补 `WasmCallCost` base（100 恒收 / cold +2500 并 warm 地址 / value≠0 且目标 empty +25000 / value≠0 +9000，逐项对 budget 检查，超了烧光），63/64 改回 nitro 的 `(gasLeft-baseCost)*63/64`（**不等于** `x - x/64`，x=65 时 63 vs 64），stipend 加在 63/64 之后且计入 cost，static+value 直接拒。② create 补 `CreateGas`（CREATE2 加 keccak word cost，revm `create2_cost(len)` 正好等价），预算不足返 `out of gas` 错误串，withheld 1/64 退还调用方。③ AccountCode 补 EXTCODESIZE 分量（cold 3300 / warm 800），买不起时返空 code 但全额计费。④ SetTrieSlots 按请求头的 gasLeft 逐 slot 检查——**用 per-slot journal checkpoint 解决"revm 必须先写才知道 cost"**：不够则 revert 该 slot、置 0、报 OutOfGas（ArbOS≥50）。⑤ return-data EVM 平价：nitro 是**给返还 gas 设上限**不是再收费（`contract.Gas = min(gas, startingGas - evmCost)`，`startingGas` 是预扣**前**的帧 gas，ArbOS≥31）。

- `[2026-07-21][done] 共识 page limit` — commit `a8c0fb6`。ArbOS≥59 的 `StylusParams.PageLimit`（默认 128）两个调用点都接上：预扣处用 `open+footprint`（写入前），AddPages 用写入后的 total。破限价 MaxUint64 → OOG（不是 revert）。
  **Decision:** 节点级 `MaxOpenPages` cap **不实现**——它是节点策略不是共识（nitro 在 on-chain 模式只 log），且默认值与 PageLimit 相同。若日后发现 writer 配了不同值再说。

- `[2026-07-21][decided] RecentWasms 当前架构做不了，偏差方向已确认` — 不是"暂缓"，是结构性限制。
  **Decision:** `ArbitrumExecutionContext` 由 `execution_env_for_tx()` **per-tx 构造**（`leafage-evm-rpc/.../arbitrum/api.rs`），而 RecentWasms 是**块内跨 tx** 的 LRU（nitro 挂在 StateDB 上，`SetTxContext` 不重置、只有新块才清）。没有块级状态可挂。**偏差是单向的**：leafage 对块中间的重复调用永远按 miss 计价，比 writer 多收 `initGas ≈ 8832 + init_cost`（典型 ~14.5k gas），**只会高估不会低估**。要闭合必须引入块序重放，属于架构改动，不在本轮范围。已写进 `stylus.rs` 预扣处的注释。

- `[2026-07-21][open] trace 完整性两项未做，需要架构决策` — 都不影响 gas/执行结果，只影响 trace 可见性。
  **① `drive_subframe` 走的是非 inspect 路径**（`stylus.rs` 里调 `evm.frame_run()`）：traced 执行下，Stylus 程序发起的子调用**整棵子树对 inspector 不可见**，DeBank 的 `pre_traceMany`/`simulateTransactions` 会丢这部分 call tree。这是 PR #184 B2 的同类问题再下沉一层。**卡点**：`inspect_frame_run` 要求 `I: Inspector<...>`，而 `run_stylus_frame`/`drive_subframe` 目前是无约束泛型，`EvmTr for ArbitrumEvm<DB, I>` 的 impl 也不带该约束。要么拆 traced/untraced 两条驱动路径，要么给整条链路加 Inspector 约束（影响公共 API）。**需要定方案再动。**
  **② CaptureHostIO(14) + `EvmData.tracing`**：现在 tracing 恒 false，所以 Rust 侧根本不发 request 14，当前的 no-op **是安全的、gas 零影响**（`req.rs:330` 连 cost 都丢弃）。要接 trace 得先把 tracing 置真，再移植 nitro `CaptureEVMTraceForHostio` 那套 hostio→等价 EVM opcode 的翻译（`arbos/util/tracing.go`，几百行，还有 call/write_result 等特例）。工作量与 ① 同级，建议一起定。

- `[2026-07-21][open] clippy 既有 error（非本次引入）` — `arbitrum/precompile/owner.rs:1121` 报 `absurd_extreme_comparisons`（`resource <= RESOURCE_KIND_UNKNOWN` 恒真/恒假）。
  **Decision:** 已用 `git stash` 验证 base 上同样报错，与 Stylus 改动无关（疑似本地 clippy 版本比当初新）。不在本分支顺手改（跨模块、非本次请求范围），需要单独处理。
