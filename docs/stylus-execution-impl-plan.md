# leafage-evm Stylus 执行实现计划 (Implementation Plan)

状态：计划（待审批）。配套设计文档：[`docs/stylus-execution-design.md`](./stylus-execution-design.md)。
实现基线：**PR #184**（分支 `fix/arb-evm-opcode-env`，tip `b16d331`，target `arb-evm`）。所有 leafage 行号锚点基于该分支的 worktree `.../leafage-evm.worktrees/fix-arb-env`。
参照实现：Chaintable/nitro `v3.11.1-debank-2`（commit `8c6468aa2`，写节点）+ Chaintable/go-ethereum-arb（`debank` 线，pipeline）。
依赖版本（已 pin）：`revm 36.0.0` / `revm-handler 17.0.0` / `revm-interpreter 34.0.0` / `revm-inspector 17.0.0`（`Cargo.toml` 无 `[patch]`，revm 内部函数不可改，所有接入必须走公开 trait override）。

本计划是设计文档的**执行版**：把 §4/§5 的方向落成文件级任务、验收标准与依赖顺序，并把两轮对抗性验证发现的残余风险写进各 Phase 的验收门。

---

## 0. 本计划相对设计文档的四点增量（研究后确认）

1. **接入缝已定并经对抗验证——`EvmTr::frame_run` + `InspectorEvmTr::inspect_frame_run`**（设计 §4.4 标注"需先验证"的点）。两名独立 skeptic 对着 revm 36 真实源码尝试证伪，均未能推翻：签名逐字匹配、callee bytecode 在该缝可见、合成 `InterpreterResult` 能正确回传父 CALL 的 gas/output/revert。见 §1.1。
2. **重大 scoping 澄清——leafage 不重写 ink 计量**。leafage 链接同一份 `libstylus`，因此 (a) 帧边界 gas↔ink 换算、(b) 每个 hostio 的 ink 计量 + 固定 `HOSTIO_INK` 开销，都在 pin 的 Rust 库内部完成。leafage 只需：帧级预扣（设计 §4.6 的 memory-init + init/cached cost）、把 15 个 hostio 的 **EVM gas cost** 按 nitro Go `api.go` 语义算出来、return-data 平价、refund 累加、page 模型、RecentWasms。这把设计 §4.6 的难度显著降低。见 §1.2。
3. **native lib pin 目标修正**——nitro 当前 HEAD 领先最新不可变 tag `v3.11.1-debank-2` 两个 commit；pin 用 **commit `8c6468aa2` 或 tag `v3.11.1-debank-2`**，不要用 `v3.11.1-debank` 这种不存在的裸名。且现有代码只绑了 `stylus_activate` + `free_rust_bytes`（2 个符号，不是 5 个）。见 Phase 0。
4. **对抗验证暴露的三处残余风险**已写进验收门：traced 路径必须同步 override（PR #184 B2 同类）、refund 必须显式 thread、Stylus 子调用必须同步驱动且手动 pop（G1）。见 §10 风险登记表。

---

## 1. 关键架构决策（已验证）

### 1.1 接入缝：`frame_run` / `inspect_frame_run`（方案 b，body 替换）

revm 36 帧生命周期由 `Handler::run_exec_loop`（`revm-handler-17.0.0/src/handler.rs:389-420`）驱动，循环调 `EvmTr` 的 `frame_init` → `frame_run` → `frame_return_result`。leafage 已 override `frame_init`（`evm/mod.rs:148`）和 `frame_run`（`evm/mod.rs:156`，目前是 passthrough）。

**选 `frame_run` 而非 `frame_init`：** 到 `frame_run` 时，revm 的 `make_call_frame`（`frame.rs:135-247`）已经替我们完成了 depth 检查、journal checkpoint、value transfer、precompile 优先级、callee bytecode 载入、`interpreter.gas = Gas::new(forwarded)`。在 `frame_run` 里 callee code 可从 `frame.interpreter.bytecode.original_byte_slice()` 读到，命中 `0xEFF0` 前缀就跑 WASM、否则 `self.inner.frame_run()`。跑完把合成的 `InterpreterAction::Return(interp_result)` 喂回 **同一个** `frame.process_next_action(ctx, action)`——checkpoint commit/revert、`FrameResult::Call(CallOutcome)` 包装、父帧 return 语义全部免费继承。

拒绝方案 (a) 在 `frame_init` 短路：`make_call_frame` 对非空 bytecode 返回 `ItemOrResult::Item`（已 push 帧），此时再返回 `Result` 会让 `run_exec_loop`（`handler.rs:405-410`，"does not pop the frame"）留下损坏的帧栈；要么就得在 revm 外重实现 depth/checkpoint/transfer 一整套 fragile 内部逻辑。拒绝方案 (c) 自定义 opcode：Stylus 首字节 `0xEF` 是 INVALID，没有单 opcode hook 能给出整 body 语义。

**双路径要求（强制，非可选）：** traced 执行走的是 `InspectorHandler::inspect_run_exec_loop` → `evm.inspect_frame_run()`（`revm-inspector-17.0.0/src/traits.rs:143`），不是 `frame_run`。默认 `inspect_frame_run` 对 `EthFrame` 永远返回 `Some(self)`、跑 `inspect_instructions`（EVM opcode 循环），**不会** fallthrough 到 `frame_run`。所以只改 `frame_run` 会让所有 traced RPC（`pre_traceCall`/`pre_traceMany`/`simulateTransactions`）把 Stylus 合约当坏 EVM 跑——这就是 **PR #184 B2 同一类 bug，只是下沉一层**。必须同时 override `inspect_frame_run`，且保留 inspector 的 `frame_end`/`call`/`call_end` hook（否则 tracer 看到有头无尾的 call 帧）。leafage 已经在上一层用 `inspect_execution`（`handler.rs:769`）打过这个补丁，本次是同构地在 `*_frame_run` 层再打一次。

**G1——WASM 子调用不能 yield（最难的子问题）：** revm 用"从 `frame_run` 返回 `InterpreterAction::NewFrame` → `run_exec_loop` 建子帧 → 用 `return_result` 恢复父帧"表达子调用；native WASM（wasmer）执行栈无法这样 unwind/resume。因此 Stylus 的子调用（CALL/DELEGATE/STATIC/CREATE）必须**从 hostio 内部同步驱动**：在同一个 `FrameStack` 上 `frame_init(child)` + 内层 `frame_run`/`frame_init`/`frame_return_result` 循环跑完子树，**但绝不能让子树最终结果走通用 `frame_return_result`**（它会对现在栈顶的 Stylus 父帧调 `return_result`、篡改父帧 `interpreter.gas`/EVM stack，破坏 ink 记账）。正确做法：直接捕获子 `FrameResult::Call(CallOutcome)`、手动 `FrameStack::pop`、读 `out.result.gas.remaining()`/`out.result.output`，自己折回 WASM ink 预算。两个配套细节：① `FrameStack::get_next` 在满时 `reserve(8)` 会 realloc backing Vec、悬垂之前 `get()` 拿的 `&mut frame`（raw-ptr backed，`local.rs:110-123`）——**驱动子调用前必须 drop 父帧 borrow、之后重新 `get()`**，这是 soundness 要求不是风格；② 建子 `FrameInit` 要带 `memory: interpreter.memory.new_child_context()` 和 `depth+1`（`frame.rs:394-397`）。

**G4——bytecode 形态：** leafage 必须把激活的 Stylus 程序存成 raw legacy `Bytecode`，让 `0xEFF0` 前缀在 `original_byte_slice()` 里存活。已验证 `0xEFF0` ≠ EOF magic `0xEF00` ≠ EIP-7702 `0xEF01`，走 `new_legacy`→`LegacyAnalyzed`、不被 revm 重解析或 EIP-3541 拒绝（3541 只在 CREATE 时拒 `0xEF`，ArbOS 注入的程序 bypass）。这是 leafage 侧不变量，不是 revm 障碍。

### 1.2 计量委托——最大的 scoping 收窄

`stylus_call` 的 `gas: *mut u64` 是 INOUT：传入 forwarded gas，Rust 内部 `ink = gas_to_ink(gas)` 跑完 `*gas = ink_to_gas(ink_left)` 写回剩余 gas（`crates/stylus/src/lib.rs:283-314`）。因此：

- **leafage 帧边界 gas 数学 = `record_cost(forwarded - *gas_written_back)`**，等价 nitro 的 `startingGas - contract.Gas`。ink 换算与舍入方向都在 pin 的库内部，天然与 nitro 一致（同一份代码）。
- **每个 hostio 的 ink 计量 + 固定 `HOSTIO_INK(8400)` + `PTR_INK`** 也在库内部（`crates/wasm-libraries/user-host-trait`）。leafage 的 `handle_request_fptr` 回调只需返回该 hostio 的 **EVM gas cost**（nitro Go `api.go` 语义），库自己 `buy_ink(gas_to_ink(cost) + HOSTIO_INK)`。

**因此 leafage 只负责**（都在 leafage/revm 侧，用 revm 现成 gas helper 保证 2929/2200 精确）：
1. 帧级**预扣**（进 WASM 前）：memory-init cost + program init/cached cost + page-limit penalty（设计 §4.6 / 本文 §6）。
2. 15 个 hostio 的**状态副作用 + EVM gas cost 计算**（nitro `api.go` closure 的 Rust 移植；本文 §5）。
3. WASM 返回后的 **return-data EVM 平价**后扣（`programs.go:289-303`）。
4. **refund 累加**（hostio SSTORE 等）并显式 set 到结果 `Gas`（Gap 1，见 §10）。
5. **page 模型**（`memory.go` 的 `[129]uint32 memoryExponents` 表逐字嵌入）与 **RecentWasms**（§7）。

**注意双重计费陷阱**：`EmitLog`、`TLOAD`、`TSTORE` 的 Go 侧 wire cost 返回 **0**（Rust 侧已用 `pay_for_evm_log`/`TLOAD_GAS`/`TSTORE_GAS` 计过）——leafage 回调对这三项也必须返回 0，不能重复收。

### 1.3 数据模型（设计 §3 结论复述）

不新增任何持久化/pipeline 字段。执行的全部共识输入 leafage 已复制（链上 WASM、`codeHash→moduleHash`、StylusParams、arbos version、激活元数据）。唯一"净新增"是**本地派生的 native-asm 缓存**（从链上 WASM 用 `stylus_compile` 确定性重算，键 `(module_hash, target)`），纯性能、可后置——首版每次 call 现场 compile 也能跑。

---

## 2. Worktree / 分支 / PR 策略

按 CLAUDE.md「Git Worktree 开发隔离」+「所有代码改动走 PR」：

- 从 PR #184 分支切**新 worktree**：`git -C <leafage-evm> worktree add ../leafage-evm.worktrees/stylus-exec -b feat/stylus-execution fix/arb-evm-opcode-env`。所有编译/测试/commit 在该 worktree 内，主 checkout 保持只读。
- **stacked PR，逐 Phase 提**，base 指向 `arb-evm`（PR #184 合并后）或临时指向 PR #184 分支。每个 Phase 一个可 review 的 PR，绝不把 5 个 Phase 揉成一坨。
- 移植 nitro 代码严格遵循 CLAUDE.md「Port verbatim」：先 1:1 复制 `api.go` closure / `memory.go` 表 / pre-charge 顺序，再适配 revm 类型；每个调用追到定义确认具体常量值（`MinInitGasUnits=128`、`CostScalarPercent=2`、`InkPrice` 默认 10000 等）。
- 动代码前先读 `~/code/andrej-karpathy-skills/CLAUDE.md`（本 session 已读）。
- 实现期的决策/卡点/实测值写进本项目 `docs/todo.md`（新开 Stylus 段），不散落 conversation。

---

## 3. 分阶段实施（0–4，每 Phase：目标 / 文件 / 任务 / 验收门 / 风险）

### Phase 0 —— cdylib 构建 + moduleHash 复现（前置门）

**目标**：产出可 dlopen 的 `libstylus.{so,dylib}`，并实测 leafage 侧 `stylus_activate` 产出的 `module_hash` == 链上 Programs `{2}` 里的 moduleHash。这是后续一切的门。

**文件（nitro 侧）**：`crates/stylus/Cargo.toml`、`Makefile`。

**任务**：
1. `crates/stylus/Cargo.toml` 的 `[lib] crate-type` 从 `["lib","staticlib"]` 改为 `["lib","staticlib","cdylib"]`（Option A，一行）。`prover_ffi` 的 re-export（`lib.rs:22-24`）已把 `free_rust_bytes` 等 `#[no_mangle]` 符号带进任何链接产物，cdylib 同样继承，无需新 crate。风险：cdylib 拉全依赖图的 `#[no_mangle]` 可能撞重复符号——`cargo build -p stylus --lib` 后 `nm -D` 验证；只有 `prover-ffi`（`crate-type=["lib"]`）定义这些符号，撞的概率低。若真撞，退到 Option B（薄 wrapper crate）。
2. Makefile 加一个 `.so/.dylib` target（镜像 `Makefile:460-463`，按 `uname -s` 选后缀），或直接 `cargo build --profile stripped --lib -p stylus`（host arch，一次同时出 `.a` 和 `.{so,dylib}`）。
3. **pin**：nitro checkout 到 commit `8c6468aa2`（或在该 commit 打不可变 tag `v3.11.1-debank-3`）。**必须确认写节点镜像跑的 nitro commit == 该 commit**（读 writer 镜像 tag / `debug.Version`）——目前是 INFERRED，上线前 SRE 侧核实。记录 dylib 的 nitro commit + sha256 到 leafage（如 `docs/todo.md` + 一个 const）防 ABI 漂移。
4. macOS 开发用绝对路径 `LEAFAGE_ARB_STYLUS_LIB=/abs/.../libstylus.dylib`（`libloading::Library::new` 用绝对路径绕开 rpath，无需 `install_name_tool`）。

**验收门（设计 §5 Phase 0 / §8.4）**：目标链一个真实已激活程序，leafage `stylus_activate` 产出 `module_hash == 链上 {2}`；`cargo build -p stylus --lib` 出 cdylib 且 `nm -D` 含全部 16 个 `stylus_*` + `free_rust_bytes`。**此门不过不进 Phase 1。**

**风险**：写节点 nitro commit 与 pin 不一致 → 激活产物（moduleHash/init_cost）与 writer 不 byte-identical。缓解：Phase 0 一次性实测断言。

---

### Phase 1 —— FFI 绑定（call/compile/target_set）+ native-asm 缓存 + hostio 回调脚手架 + 降级安全

**目标**：能对一段 fixture WASM `compile` 出 native asm 并 `call` 返回（先 mock hostio）；`LEAFAGE_ARB_STYLUS_LIB` 未设时非 Stylus 执行完全正常、CALL Stylus 返回清晰 `Unconfigured` 错误不 panic。

**文件**：`arbitrum/precompile/stylus_runtime.rs`（扩 FFI）、`arbitrum/evm/context.rs`（native-asm 缓存）、`arbitrum/precompile/state/stylus.rs`（新增 `wasm_module_hash` 读取器）、新模块 `arbitrum/evm/stylus/`（hostio 回调 + 上下文）。

**任务**：
1. **FFI mirror**（照 §4 checklist，逐字段匹配 repr/顺序/padding）：在 `stylus_runtime.rs` 加 `StylusCallFn` / `StylusCompileFn` / `StylusTargetSetFn` 三个 `extern "C"` typedef + 通过现有 `symbol::<T>()` loader 解析；加 `#[repr(C)]` 结构 `RustSlice`、`NativeRequestHandler`（**bare fn-ptr + id，不是 vtable**）、`StylusConfig`（`version:u16` 后有 **2 字节 pad**）、`PricingParams`、`EvmData`（16 字段严格按 §4 顺序）、`Bytes20`。复用现有 `GoSliceData`/`RustBytes`/`Bytes32`/`StylusData`。`UserOutcomeKind` u8（0 Success/1 Revert/2 Failure/3 OutOfInk/4 OutOfStack/5 NativeStackOverflow）。
2. **进程级 target 初始化**：host 首次调 `stylus_target_set`（linux arm64=`"arm64-linux-unknown+neon"`；x86_64=`"x86_64-linux-unknown+sse4.2+lzcnt+bmi"`；`native=true`）。macOS 开发传空 description → 用真实 host `Target::default()`（`target_cache.rs:17`）。注：只有 `stylus_compile`/`stylus_call` 需要 target_set，`stylus_activate` 走 `TARGET_NATIVE` 默认值不需要。
3. **native-asm 缓存**：`ArbitrumExecutionContext`（`context.rs:27` 旁）加 `compiled_asm: HashMap<(B256,Target), Bytes>` + 访问器（镜像 `insert_activated_wasm_module`/`activated_wasm_module`）。miss → 解压链上 WASM（复用 `wasm.rs` decode 路径）→ `stylus_compile(wasm, program.version, target, cranelift=false)` → 回填。
4. **新状态读取器 `wasm_module_hash(code_hash) -> B256`**（`state/stylus.rs`）：目前只有 writer `save_wasm_module_hash`（`:309`），无 reader；call path 拿 callee code_hash 后需读 module_hash 去查 native module。
5. **hostio 回调脚手架**：定义 `HostioCtx`（持 `&mut ArbitrumContext<DB>` 的必要片段 + refund 累加器 + readOnly 快照 + 一个 buffer arena）。`handle_request_fptr` 是 `extern "C" fn(id:usize, req_type:u32, *mut RustSlice, *mut u64, *mut GoSliceData, *mut GoSliceData)`；`id` 传 `*mut HostioCtx`（usize 装指针），回调把 `id` 转回 `*mut HostioCtx`。**ABI 陷阱**：Rust 侧惰性持有 `raw_data`（`GoSliceData`，`DataReader::slice()` 懒读），所以回调写入的 result/raw_data buffer 必须活到整个 `stylus_call` 返回——`HostioCtx` 的 buffer arena 负责。Phase 1 回调可先只实现 return/mock，返回固定 gas。
6. **降级安全**：`activate_from_env` 的 `Unconfigured` 路径已有；dispatch 分支在 runtime 缺失时返回清晰错误的 `InterpreterResult`（Revert + 错误消息），不 panic、不污染其他执行。

**验收门**：`stylus_compile→stylus_call`（真 libstylus + fixture `.wat`，如 nitro `crates/stylus/tests/add.wat`）roundtrip 通过；env 未设时全 corpus + 全单测通过（§8.2.7）。

---

### Phase 2 —— dispatch 缝 + 最小 hostio + 预扣 + gas/refund thread

**目标**：跑通一个只读/纯存储的 Stylus 合约，return / gasUsed 对齐 writer。回归门：非 Stylus 路径逐字节不变。

**文件**：`arbitrum/evm/mod.rs`（override `frame_run` + 新 `inspect_frame_run`）、新模块 `arbitrum/evm/stylus/{mod,dispatch,exec}.rs`、`arbitrum/evm/context.rs`、`wasm.rs`（前缀 predicate 提升可见性）。

**任务**：
1. **前缀 predicate**：把 `wasm.rs:32-35` 的 `STYLUS_*_PREFIX` + `STYLUS_HEADER_LEN` + `is_stylus`（`wasm.rs:621`）提到 `evm/` 与 `precompile/` 共享的位置（或在 `evm/stylus` 重声明），byte-exact 匹配 nitro `IsStylusProgramPrefix`（3 种前缀 + `code.len()>prefix.len()` + arbos≥30 gate）。**gate 只是前缀+版本**；激活有效性（version!=0、未过期、version 匹配）在分支内 `active_wasm_program`（`state/stylus.rs:257`）检查，无效则按 nitro 返回对应错误（ProgramNotActivated/NeedsUpgrade/Expired）。
2. **`frame_run` override**（替 `mod.rs:156-160`）：`let code = self.inner.frame_stack.get().interpreter.bytecode.original_byte_slice();` 命中 `is_stylus(code)` → 走 `stylus::exec::run(...)`；否则 `self.inner.frame_run()`。exec 内：读 `frame.interpreter.{input,gas,runtime_flag}` → **drop 帧 borrow** → 预扣（§6）→ 组装 `EvmData`（§4.3）→ compile/缓存查 → `stylus_call` → 组 `InterpreterResult` → 重新 `get()` 帧 → `frame.process_next_action(ctx, InterpreterAction::Return(result)).inspect(|i| if i.is_result(){frame.set_finished(true)})`。
3. **`inspect_frame_run` override**（新增到 `mod.rs:234` 的 `InspectorEvmTr` impl 块）：同样的 Stylus 分支，但保留 inspector 的 `call`/`call_end`/`frame_end` hook（`traits.rs:158-166`）——否则 traced RPC 把 Stylus 当坏 EVM 跑。**这是 PR #184 B2 同类，验收必须证明 traced==untraced。**
4. **gas thread**（§1.2）：以 forwarded limit seed 一个 `Gas`；`record_cost(pre_charge)`→false 则 `spend_all()` + `OutOfGas` result；把预扣后 remaining 作为 `*gas` 传 `stylus_call`；`record_cost(fed - *gas_back)`；`record_cost(return_data_parity)`；**`record_refund(hostio_refund_累加)`**（Gap 1，不能漏）；result 变体由 `UserOutcomeKind` 映射（Success→`Return`、Revert→`Revert`、OutOfInk/OutOfStack→`OutOfGas`/`Revert`，**永不发 `FatalExternalError`**，它在 `return_result` 会 panic）。
5. **最小 hostio**：`GetBytes32`(SLOAD)、`SetTrieSlots`(SSTORE，含 refund 累加)、return data 写入。**复用 revm 现成 gas helper**（`sload_cost`/`sstore_cost`/warm-cold）算 EVM gas cost，别手撸 2929/2200。wire 格式严格按 §5（`take*` 逐字节消费，多余字节 nitro 会 panic）。
6. **wire 起 `stylus_pages_open`/`activated_wasm_modules`**：这些字段目前 prod 零消费者/永不写（context.rs），本 Phase 起真正读写。

**验收门**：
- 只读/纯存储 fixture（nitro `storage`/`keccak`）CALL，return + gasUsed 对齐 writer。
- **回归（§8.2.1-8.2.3）**：dispatch gate 单测钉死（普通 code 永不进 Stylus 分支，无假阳性）；`cargo test -p leafage-evm-chains -p leafage-evm-rpc`（≥279+36）零回归；leafage-bench corpus **base vs stylus 分支逐 case byte-identical**（需先做 §8 的 `--assert-outputs-match`）。
- **traced==untraced**：同一 Stylus call 经 `pre_traceCall` 与 `eth_multiCall` 结果一致。

---

### Phase 3 —— 完整 hostio + 子调用同步驱动（G1）

**目标**：跨合约（Stylus↔Solidity 互调）、log、create、余额/code 查询、page 增长、transient、重入对齐 writer。

**文件**：`arbitrum/evm/stylus/{exec,hostio}.rs`。

**任务**：
1. **补齐 15 个 RequestType**（§5 表）：`Get/SetTransientBytes32`(cost 返回 0)、`ContractCall/DelegateCall/StaticCall`、`Create1/Create2`、`EmitLog`(cost 返回 0)、`AccountBalance/Code/CodeHash`、`AddPages`、`CaptureHostIO`(接 inspector，非 trace no-op)。每项复用 revm gas helper，readOnly 在 handler 构造时快照、传播给该帧所有 request（nitro `api.go:73` 只读一次）。
2. **子调用同步驱动（G1，最 delicate）**：`ContractCall/Delegate/Static` 和 `Create1/2` 在 hostio 内：drop 父帧 borrow → 建 `FrameInit{ input: CallInputs/CreateInputs, depth: parent.depth+1, memory: parent.memory.new_child_context() }`（自己算 63/64、+2300 stipend、static 传播）→ `self.inner.frame_init(child)` + 内层 `frame_run`/`frame_init`/`frame_return_result` 循环跑完子树 → **捕获顶层子 `CallOutcome`、手动 `FrameStack::pop`**、读 `gas.remaining()`/`output`/`gas.refunded()` 折回 → 重新 `get()` 父帧。**绝不让子结果走通用 `frame_return_result`**（会篡改 Stylus 父帧 gas）。code 查询命中的 callee 可能又是 Stylus（递归）。
3. **`AddPages` 接 page 模型**（§7）：`memoryExponents[129]` 表逐字嵌入，`set_stylus_pages_open` 增长、指数计费、page-limit 破限返 `MaxUint64` 强制 OOG。

**验收门**：hostio 矩阵（纯计算/存储/跨合约双向/log/create/余额/重入，用 nitro `crates/stylus/tests/` 的 `multicall`/`evm-data`/`log`/`create`/`hostio-test`/`erc20`）；与 writer `debug_traceCall` 的子调用树/logs/storage 变更对齐（§8.1.3）。

---

### Phase 4 —— ink/page/gas/cached 精确对齐 + writer 逐 case 对账

**目标**：一组真实历史 tx 的 gasUsed / trace 与 writer 一致（cached caveat 内）。

**文件**：`arbitrum/evm/stylus/precharge.rs`、`context.rs`（RecentWasms）。

**任务**：
1. **预扣精确化**（§6）：memory-init（footprint）+ init/cached cost + page-limit penalty，一次 `record_cost`，顺序照 `programs.go:194-263`。常量逐字核（`MinInitGasUnits=128`、`MinCachedGasUnits=32`、`CostScalarPercent=2`、默认 `InkPrice=10000`、`InitialFreePages=2`/`InitialPageGas=1000`/`initialPageLimit=128`）。
2. **return-data EVM 平价后扣**（`programs.go:289-346`）：`evmMemoryCost(len(ret))`，ArbOS≥StylusFixes 生效。
3. **cached 标志 / RecentWasms**（§7、设计 §4.7）：首版走**策略 A**（`cached` 只取链上 `program.cached`，忽略块内 LRU），文档标注块中间 tx 的 init gas 可能差一档；**策略 B**（整块重放维护 ArbOS≥60 的块内 LRU，seed `BlockCacheSize`，随 StateDB checkpoint snapshot/restore）留给 bit-精确 gas 场景。
4. **refund/ink 舍入终检**（§10 Gap 1/2）：确认 hostio SSTORE refund 全程 thread 到 tx 级（`last_frame_result`→`reward_beneficiary` 的 `used=spent-refunded`）；ink→gas 舍入在 pin 库内、天然对齐 nitro。

**验收门**：真实历史 tx 的 gasUsed/trace 与 writer 逐 case 一致（cached caveat 内）；consistency checker（非 Stylus 块一致率不回归）+ 自定义 RPC（非 Stylus tx 不变）+ perf（前缀检查开销在噪声内）。

---

## 4. FFI mirror checklist（照 nitro，加进 `stylus_runtime.rs`）

已有：`GoSliceData{ptr,len}`、`RustBytes{ptr,len,cap}`、`Bytes32([u8;32])`、`StylusData`（8 字段）、`StylusActivateFn`、`FreeRustBytesFn`、`symbol::<T>()` loader。**新增**（`#[repr(C)]`，逐字段匹配，禁 pack）：

1. `RustSlice{ptr:*const u8, len:usize}`（丢 PhantomData，16 字节）。
2. `Bytes20([u8;20])`（align 1）。
3. `NativeRequestHandler{ handle_request_fptr: unsafe extern "C" fn(usize,u32,*mut RustSlice,*mut u64,*mut GoSliceData,*mut GoSliceData), id: usize }`。
4. `PricingParams{ ink_price: u32 }`；`StylusConfig{ version:u16, /*2B pad*/ max_depth:u32, pricing:PricingParams }`（12 字节）。
5. `EvmData`（16 字段，**严格顺序**）：`arbos_version:u64, block_basefee:Bytes32, chainid:u64, block_coinbase:Bytes20, block_gas_limit:u64, block_number:u64, block_timestamp:u64, contract_address:Bytes20, module_hash:Bytes32, msg_sender:Bytes20, msg_value:Bytes32, tx_gas_price:Bytes32, tx_origin:Bytes20, reentrant:u32, return_data_len:u32, cached:bool, tracing:bool`。
6. `StylusCallFn = unsafe extern "C" fn(GoSliceData/*module*/, GoSliceData/*calldata*/, StylusConfig, NativeRequestHandler, EvmData, bool/*debug*/, *mut RustBytes/*out*/, *mut u64/*gas INOUT*/, u32/*long_term_tag*/) -> u8`。
7. `StylusCompileFn = unsafe extern "C" fn(GoSliceData/*wasm*/, u16/*version*/, bool/*debug*/, GoSliceData/*target*/, bool/*cranelift*/, *mut RustBytes/*out*/) -> u8`。
8. `StylusTargetSetFn = unsafe extern "C" fn(GoSliceData/*name*/, GoSliceData/*desc*/, *mut RustBytes/*out err*/, bool/*native*/) -> u8`。

**ABI 陷阱**：`RustBytes` 带 `cap`、Rust 拥有须 `free_rust_bytes` 释放；`GoSliceData` 是借来的、Rust 不释放但**惰性持有 raw_data**（buffer 须活到 `stylus_call` 返回）；`StylusConfig`/`StylusData` 有内部 pad；`bool` 1 字节；回调 `req_type` 已带 `+0x10000000` offset，handler 要减回。`EvmData.block_number` = **L1 块号**（设计 §1.6，来自 `arbos_state.blockhashes_l1_block_number()`，与 PR #184 的 BLOCKHASH/NUMBER 语义一致）；`block_basefee`/`block_number`/`block_timestamp` 用 `ArbStorage::current_l2_*` 的 nitro 调整值而非 raw `ctx.block()`。

---

## 5. Hostio RequestType → revm 映射（15 项，wire + gas，照 nitro `api.go`）

回调必须**逐字节精确消费** input（多余字节 nitro 侧 panic）。gas 是 EVM gas（库内转 ink）。

| # | RequestType | input | output(response/raw_data) | gas cost | revm 实现 |
|---|---|---|---|---|---|
|0|GetBytes32|key[32]|value[32]/nil|`WasmStateLoadCost`(cold/warm SLOAD 2929)|`journal.sload` + revm `sload_cost`|
|1|SetTrieSlots|gasLeft[8]++(key[32]++value[32])×N|status[1]/nil|Σ`WasmStateStoreCost`(2929+2200 refund)|`journal.sstore` + `sstore_cost`；**refund 累加**|
|2|GetTransientBytes32|key[32]|value[32]/nil|**0**|TLOAD|
|3|SetTransientBytes32|key[32]++value[32]|status[1]/nil|**0**|TSTORE|
|4|ContractCall|addr[20]++value[32]++gasLeft[8]++gasReq[8]++calldata|status[1]/ret|`base + (gas-returnGas)`；base=`WasmCallCost`；63/64；value≠0 加 CallStipend|§3 子调用驱动|
|5|DelegateCall|同上|status[1]/ret|同 doCall|同上（code vs storage 地址）|
|6|StaticCall|同上|status[1]/ret|同 doCall|同上（static 传播）|
|7|Create1|gas[8]++endowment[32]++code|成功 1++addr[20]/ret；失败 0++err/nil|`CreateGas` + 63/64（refund 1/64）|CREATE|
|8|Create2|gas[8]++endowment[32]++salt[32]++code|同上|+`Keccak256WordGas*words`|CREATE2|
|9|EmitLog|topics[4]++hash[32]×topics++data|空/nil|**0**（Rust 侧 `pay_for_evm_log`）|LOGn|
|10|AccountBalance|addr[20]|balance[32]/nil|`WasmAccountTouchCost(false)`(2929)|BALANCE|
|11|AccountCode|addr[20]++gas[8]|nil/code|`WasmAccountTouchCost(true)`；gas<cost 返 cost+空 code|EXTCODECOPY|
|12|AccountCodeHash|addr[20]|codeHash[32]/nil|`WasmAccountTouchCost(false)`|EXTCODEHASH|
|13|AddPages|pages[2]|空/nil|`memoryModel.GasCost`；破限 MaxUint64|§7 page 模型|
|14|CaptureHostIO|startInk[8]++endInk[8]++nameLen[4]++argsLen[4]++outsLen[4]++name++args++outs|空/nil|**0**（tracing）|inspector hook / no-op|

status 枚举：`Success=0, Failure=1, OutOfGas=2, WriteProtection=3`（call 成功 statusByte=0 失败=2；create 成功=1 失败=0）。readOnly 在 setTrieSlots/setTransient/create/emitLog/value-call 强制。

---

## 6. 进 WASM 前的预扣序列（leafage 侧，照 `programs.go:194-263`）

顺序 load-bearing：
1. `params = stylus_params()`；`program = active_wasm_program(code_hash, time, params)`（version/expiry 检查）；`module_hash = wasm_module_hash(code_hash)`。
2. `open,ever = pages`；`callCost = memoryModel(FreePages,PageGas).GasCost(program.footprint, open, ever)`。
3. cached 判定（§7）：`cached = program.cached || (arbos≥60 && recentWasms.Insert)`；`if cached || version>1 { callCost += cachedGas }`；`if !cached { callCost += initGas }`。
   - `initGas = MinInitGas*128 + ceil(initCost*InitCostScalar*2/100)`；`cachedGas = MinCachedInitGas*32 + ceil(cachedCost*CachedCostScalar*2/100)`。
4. `newOpen = open + footprint`；`penalty = enforceStylusPageLimit(newOpen,...)`（破限 MaxUint64）；`callCost += penalty`。
5. `record_cost(callCost)`→false 则 OOG（进 WASM 前退出）；`AddStylusPages(footprint)`；返回时 `set_stylus_pages_open(open)`（释放 footprint，保留 ever 高水位）。

program 元数据打包（单 32 字节 slot）：`version[0:2] initCost[2:4] cachedCost[4:6] footprint[6:8] activatedAt[8:11] asmEstimateKb[11:14] cached[14:15]`。

---

## 7. cached / RecentWasms / page 模型

- **page 模型**（`memory.go`）：`GasCost(new,open,ever) = linear(pageGas per page beyond freePages) + Δexp`。`exp(pages)` 查硬编码 `memoryExponents [129]uint32`（**逐字嵌入**，`1,1,1,1,1,1,2,2,2,3,...,27849408,31873999`；>128 页 MaxUint64）。exp 项只在 `newEver>ever` 新高水位时增长（重开已触页只 linear）。`PageRamp` 不参与 `GasCost`（已 bake 进表）。page 计数在 StateDB 非 journaled，随 checkpoint 显式 restore。
- **RecentWasms**（§3.4 caveat）：块内 LRU，`Insert(codeHash, BlockCacheSize) -> bool`（命中 true），容量首次 Insert 时按 `BlockCacheSize`（默认 32）lazy 定、每块丢弃、随 StateDB copy 深拷贝、随 checkpoint snapshot/restore、**仅 arbos≥60**。命中付 cachedGas、miss 付 initGas。首版**策略 A** 忽略之（接受块中间 tx init gas 差一档），**策略 B** 整块重放时维护——见 §9 待定。

---

## 8. 测试方案（映射真实设施 + 门）

三层设施（设计 §8）真实位置：in-crate 单测（`arbitrum/**/*.rs` 的 `mod tests`，copy `wasm.rs:896 context_with_stylus_params` + `account_info_with_code` + `with_storage`）；leafage-bench corpus 回放（`bin/leafage-bench/`）；consistency checker = 外部 Go 工具 `dettack chaintest`（`--local=<leafage> --rpc=<writer>`，输出到 `rpc_verification/`）。

**必须先补的设施缺口**（否则回归门跑不了）：
1. **leafage-bench 无输出对比**——加 `--assert-outputs-match`（`run.rs`/`bench.rs`，~40 LOC，按 `case_id` 建 map 比 target vs compare 的 `result`，mismatch 则非零退出）。或用 Option A 外部 `jq` diff 两次 `--verbose --output-dir` 的 dump（按 `case_id` sort 防 JoinSet 乱序）。
2. **runner 忽略 `block_number`**（`runner/mod.rs:88 BlockId::latest()`）——历史 Stylus/状态相关 call 要 pinned 回放需加 `--use-case-block`（独立小改）；base-vs-stylus 同 head diff 不需要。
3. **corpus 零 Stylus 样本**（700 全 `0x1`）——从 **writer 侧成功调用**采一份独立 `stylus_corpus.json`（leafage 现在 Stylus call 会失败、从没进过 corpus）。
4. **libstylus.so 只有 staticlib**——Phase 0 出 cdylib 前，FFI/fixture/writer-diff 层跑不了。
5. **dettack CheckHistory\* 只 diff 块体+余额**，不覆盖 eth_call return/gasUsed——Stylus 正确性对账用 `node-rpc-testing` skill 逐字段 vs writer，或 leafage-bench `--compare=<writer>` 输出 diff。

**测试层 ⇄ libstylus 需求**：storage/metadata/dispatch/降级 单测**不需要** .so（纯 CacheDB / env 未设）；FFI roundtrip / fixture / writer-diff **需要** .so（`LEAFAGE_ARB_STYLUS_LIB=... cargo test -p leafage-evm-chains stylus`）。CI 分裂：mock 层普通 CI；FFI 层 CI 镜像带版本 pin 的 `libstylus.so`。

**门**：Phase 0 moduleHash 复现（前置）；merge 前置 = corpus 输出 byte-identical + 全单测绿 + traced==untraced；nightly 差分 = 对 writer 跑 Stylus corpus（正确性）+ 非 Stylus 一致率（回归）。

---

## 9. 待定决策（需用户/实现期解决，进 `docs/todo.md`）

1. **cached/RecentWasms 首版策略 A 还是 B**——A 简单但块中间 tx init gas 可能差一档，B 精确但需整块重放引擎状态。建议**首版 A**，Phase 4 视 writer 对账偏差决定是否上 B。
2. **native-asm 落盘缓存是否首版做**——首版每次 call 现场 `stylus_compile`（慢但正确）；落盘（`(module_hash,target)` 本地 KV）纯性能、建议后置。
3. **plan 文档与实现分支落位**——本文与设计文档现在主 checkout `docs/`（untracked）；实现在 `feat/stylus-execution` worktree（off PR #184）。是否把两份 doc 一并 commit 进实现分支？
4. **写节点 nitro commit 核实**——Phase 0 pin `8c6468aa2` 是否 == writer 实跑 commit，需 SRE/镜像侧确认（INFERRED）。
5. **hood 当前无 Stylus 合约**（设计 §8.3 已验证 `0x71` 全时段零调用）——本功能对 hood 当前非必需，价值在 ① Robinhood 后续部署 ② 代码复用到 Arb One/其它 Orbit 链。上线前重跑该检查。是否仍按全 Phase 推进，还是先做 Phase 0-1 占位、Phase 2-4 等有真实 Stylus 链再推？

---

## 10. 残余风险登记表（对抗验证产出，已写进各 Phase 验收门）

| 风险 | 状态 | 落地 |
|---|---|---|
| **接入缝可行性** | **CONFIRMED，未能证伪** | `frame_run`+`inspect_frame_run` 签名逐字匹配、bytecode 可见、`InterpreterResult` 正确回传父 CALL 与 tx 级 gas/output/revert |
| **traced 路径 bypass**（PR #184 B2 同类） | **高危，必须防** | 默认 `inspect_frame_run` 对 EthFrame 不 fallthrough；Phase 2 强制 override 且保 `frame_end` hook；验收 traced==untraced |
| **Gap 1 refund 丢失** | **必须显式 thread** | hostio SSTORE refund 累加 → `gas.record_refund` → `last_frame_result` → `reward_beneficiary(used=spent-refunded)`；Phase 2 起接、Phase 4 终检 |
| **Gap 2 ink→gas 舍入** | **在 pin 库内、天然对齐** | `*gas` INOUT 由 `stylus_call` 内部 `ink_to_gas`，leafage 只 `record_cost(forwarded-*gas)`；确认不在 leafage 侧再除 |
| **G1 子调用同步驱动** | **最 delicate，强制** | hostio 内手动 `frame_init`+循环+`pop`+捕获 `CallOutcome`，**不走** `frame_return_result`；drop/re-`get()` 防 realloc 悬垂；子 `FrameInit` 带 `new_child_context()`+`depth+1` |
| **G4 bytecode 形态** | **leafage 侧不变量** | 激活程序存 raw legacy `Bytecode`，`0xEFF0` 前缀在 `original_byte_slice()` 存活（已验证不被 revm 拒） |
| **moduleHash bit 复现** | **同库天然一致** | Phase 0 一次性实测；nitro commit 与 writer pin 一致 |
| **RecentWasms 块内 gas** | **存在（非数据字段）** | §7 策略 A/B；trace 精确对账时整块重放 |
| **hostio 语义精度** | **主要工程风险** | 逐 RequestType 对账 gas/2929/子调用边界（Phase 3-4）；wire 逐字节消费 |

---

## 附录：接入点 file:line 索引（PR #184 worktree）

- 帧缝：`arbitrum/evm/mod.rs:148`(frame_init) `:156`(frame_run，改) `:234`(InspectorEvmTr，加 inspect_frame_run)
- FFI：`arbitrum/precompile/stylus_runtime.rs`（现绑 activate `:123` + free `:124`）
- 缓存/page：`arbitrum/evm/context.rs:27`(activated_wasm_modules) `:77-88`(pages)
- 前缀：`arbitrum/precompile/wasm.rs:32-35` + `:621`(is_stylus)
- Programs 状态：`arbitrum/precompile/state/stylus.rs`（`stylus_params:26`/`active_wasm_program:257`/`save_wasm_module_hash:309`——需加 `wasm_module_hash` reader）
- EvmData 源：`arbitrum/arbos_state.rs`（`arbos_version:190`/`blockhashes_l1_block_number:216`）+ `state/mod.rs`(`current_l2_block_number/basefee`)
- 测试 harness：`wasm.rs:896`(context_with_stylus_params) / `arbos_state.rs:497`(storage-only)
- nitro 参照：`crates/stylus/src/lib.rs`(compile:160/target_set:214/call:268)、`crates/stylus/src/evm_api.rs:12`(NativeRequestHandler)、`arbutil/src/evm/mod.rs:82`(EvmData)、`prover/src/programs/config.rs:36`(StylusConfig)、`arbos/programs/api.go:30`(RequestType)、`programs.go:194`(CallProgram)、`memory.go`(page 表)、`native.go:849`(target 描述符)、`statedb_arbitrum.go:485`(RecentWasms)
