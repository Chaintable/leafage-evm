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

- `[2026-07-15][open] Phase 2b: native-asm 缓存 + wasm_module_hash reader` — 缓存 `HashMap<B256,Bytes>` keyed by module_hash（单 host 只 native target，无需 (hash,target) 复合键）；`state/stylus.rs` 加 `wasm_module_hash(code_hash)->B256` reader（现仅 writer `save_wasm_module_hash`）。均待 frame seam 消费。

- `[2026-07-15][open] Phase 2c: frame seam（frame_run + inspect_frame_run）` — 见实现计划 §1.1/§3。`evm/mod.rs` override，检测 `0xEFF0` 前缀 → 组装 EvmData（EvmData.block_number = L1 块号）+ 预扣（§6）+ compile/缓存 + `call_from_env` + 建 `InterpreterResult` + `process_next_action`。**inspect_frame_run 必须同步 override（PR #184 B2 同类）。**

- `[2026-07-15][open] Phase 2d: 最小真 hostio` — GetBytes32(SLOAD)/SetTrieSlots(SSTORE + refund 累加)/return，走 revm journal + 现成 gas helper（2929/2200）。wire 格式按实现计划 §5 逐字节消费。

### 验证前沿（CRITICAL —— 必须诚实标注）

Phase 2c 起的执行引擎是**共识级 gas/trace 代码，其正确性只能对着活的 Arbitrum writer 或 Arb One traced RPC 差分验证**（设计 §8.1.3 gold standard）。当前环境：
- **hood 目标链无 Stylus 合约**（设计 §8.3 已验证），无法本地端到端测。
- **Arb One 官方 RPC 可作 oracle**（`eth_call`/`debug_traceCall` at pinned block），但需先把 frame seam + hostio 建到能跑真实合约、并 fetch 该合约读的 storage slots。

**结论**：FFI 基础层（activate/compile/call）已建成并对着 Arb One 共识数据验证。执行引擎（frame seam + hostio + gas）是下一里程碑，**必须以 Arb One eth_call 差分作为验收测试来建**，不能盲写共识代码当"完成"。已验证 = 会编译 + FFI ABI 对；未验证 = gas/trace 与 writer 是否逐 case 一致。

## 跨 Phase 待定（迁自实现计划 §9）

- `[2026-07-15][open] RecentWasms 策略 A/B` — 首版 A（忽略块内 LRU），Phase 4 视 writer 对账偏差决定是否上 B。
- `[2026-07-15][open] native-asm 落盘缓存` — 首版每次 call 现场 `stylus_compile`；落盘后置。
- `[2026-07-15][decided] 范围` — 用户定：全量 Phase 0-4（为 Robinhood 后续 + Arb One/Orbit 复用）。hood 当前无 Stylus 合约（设计 §8.3 已验证），上线前重跑该检查。
