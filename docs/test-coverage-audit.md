# leafage-evm Tempo T3+T4 改造 — 测试覆盖审计与判断

**审计日期**：2026-05-21
**对照分支**：`feature/tempo-t3-t4-adaptation` @ 7b328d6（PR #150）
**审计方式**：spawn 一个 code-explorer subagent 独立分析，本文档对 subagent 的每个判断逐条对照源码验证后给出结论。

## 摘要

Subagent 输出 10 项可能的测试盲点（HIGH×3 / MEDIUM×4 / LOW×3）。逐条验证后：
- **2 项是真实 bug**（H-3 双写 slot、M-1 refund max=0 清零），合 PR 前应修
- **5 项是真实测试 gap**（H-2 + M-2/M-3 + L-1/L-2/L-3），优先级中低
- **2 项是 subagent 误报**（H-1 + M-4），描述错了代码事实
- **1 项是运维项**（L-3 consistency-checker 历史窗口浅）

不需要全盘接受 subagent 的判断；它的代码引用很扎实但有时把"代码差异"误判为"bug"，必须对照 writer 验证。

---

## 真实 Bug（合 PR 前应修）

### Bug-1：`verify_and_update_spending` periodic rollover 双写同一 slot

**位置**：`crates/leafage-evm-chains/src/tempo/precompile/account_keychain.rs:908-933`

**Leafage 现状**（rollover 时）：
```rust
// L912-920
if state.period > 0 && now >= state.period_end {
    state.period_end = state.compute_next_period_end(now);
    state.remaining = U256::from(state.max);
    let handler_mut = &mut self.spending_limits[limit_key][token];
    handler_mut.write(state.clone())?;        // ← 第 1 次 write（整个 2-slot SpendingLimitState）
}
// L930-932
self.spending_limits[limit_key][token]
    .remaining
    .write(remaining - amount)                // ← 第 2 次 write（remaining 单 slot，覆盖刚写的值）
```

**Writer 行为**（`/Users/lihe/code/task_tempo/crates/precompiles/src/account_keychain/mod.rs:1153-1180`）：
```rust
let mut limit_state = self.spending_limits[limit_key][token].read()?;
let mut remaining = limit_state.remaining;
let is_periodic = limit_state.period != 0;

if is_periodic && current_timestamp >= limit_state.period_end {
    let next_end = limit_state.compute_next_period_end(current_timestamp);
    remaining = U256::from(limit_state.max);
    limit_state.remaining = remaining;
    limit_state.period_end = next_end;
    // ← 只更新 in-memory，不 write
}

if amount > remaining { return Err(...); }

let new_remaining = remaining - amount;
if is_periodic {
    limit_state.remaining = new_remaining;
    self.spending_limits[limit_key][token].write(limit_state)?;   // ← 一次 write（合并）
} else {
    self.spending_limits[limit_key][token].remaining.write(new_remaining)?;
}
```

**影响**：
- SSTORE gas 计数差异：第一次 cold + 第二次 warm dirty ≈ +5,000 gas
- 在精确 `now >= period_end` 边界触发的 AA tx 的 `eth_estimateGas` 和 state diff 与 writer 不一致

**为什么 mainnet replay 没抓到**：T4-fx6 的 period=86400，对应 fixture 块 20,675,920 不一定正好在 period_end 边界。需要构造或扫描 mainnet 上恰好触发 rollover 的 tx 才能复现。

**注释自相矛盾**：L915-917 注释自称 "matches writer write-back ordering"，但实际 ordering 跟 writer 不同——这是当时实现 FU-6 时对 writer 行为的误读。

**修复方向**：把 leafage 的 rollover 路径改为合并写（mirror writer 1180 的 `write(limit_state)`）。

---

### Bug-2：`refund_spending_limit` 在 `max == 0` 时把 remaining 清零

**位置**：`crates/leafage-evm-chains/src/tempo/precompile/account_keychain.rs:966-976`

**Leafage 现状**：
```rust
let new_remaining = if self.storage.spec().is_t3() {
    let handler = &self.spending_limits[limit_key][token];
    let state = handler.read()?;
    state.remaining
        .saturating_add(amount)
        .min(U256::from(state.max))   // ← 不区分 max=0
} else {
    ...
};
```

**Writer 行为**（`account_keychain/mod.rs:1238-1247`）：
```rust
let mut limit_state = self.spending_limits[limit_key][token].read()?;
let refunded = limit_state.remaining.saturating_add(amount);
// Legacy pre-T3 rows only persisted `remaining`, so migrated keys deserialize with
// `max = 0`. Preserve that legacy behavior and only clamp rows that were configured
// with a real T3 max.
limit_state.remaining = if limit_state.max == 0 {
    refunded
} else {
    refunded.min(U256::from(limit_state.max))
};
```

**影响**：
- pre-T3 创建的 key 在 T3+ block 上调 refund 时：`max=0` → `.min(0)` → remaining 被错误**清零**
- 用户后续无法用这个 key 花费（直到 update_spending_limit 重新设置）
- writer 行为：保持 saturating_add 语义，不 clamp，remaining 正确累加

**为什么 mainnet replay 没抓到**：T4 区间内 refund 路径触发率本来就低（少量 fee refund 场景），且 pre-T3 创建的 key 在 T4 上还在用的占比小。需要专项 fixture 扫描 mainnet 找出 (pre-T3 key, T3+ block) 的 refund tx。

**修复方向**：mirror writer 的 `if max == 0 { refunded } else { refunded.min(max) }` 三元。约 10 行改动。

---

## 真实测试 Gap（影响测试 coverage 但不是代码 bug）

### Gap-1：`KeyAuthorization` deny-all RLP 编码无 byte-eq 验证

**位置**：`crates/leafage-evm-chains/src/tempo/fee_payer.rs:1249-1268`

**现状**：单测 `key_authorization_deny_all_allowed_calls_round_trips_in_length` 只断言：
- `buf.len() == deny_all.length()`（长度自洽）
- `buf.contains(&0xc0)`（包含 empty-list 字节）

**缺**：与 writer 真实 RLP 输出做 byte-eq。手写 RLP encoder 在 `Some([])` 这一边缘 case 上最容易出错（1 字节偏差会让 gas hash 不匹配）。

**风险评估**：mainnet T4 区间 159 笔 KeyAuthorized 中 0 笔是 deny-all（10 笔含 allowedCalls 都是非空）；deny-all 的实际使用率极低。即使有偏差，BASE_SCOPE_GAS 仍然加上，差异有限。

**补法**：补一个 cargo test，hardcode writer 的预期 RLP 字节作为 fixture。约 30 行。

### Gap-2：`fee_manager.rs` 整个文件 0 个 cargo test

**事实**：`grep -c '#\[test\]' fee_manager.rs == 0`。包括 FU-9 的 `set_user_token` T3+ short-circuit 路径在内的所有 fee_manager 行为都没有 cargo 单测。

**风险**：FU-9 的 mainnet replay byte-eq 不存在（user_tokens.read 的 storage 路径未跑 fixture）。后续修改 fee_manager 任何函数都没有回归保护。

**补法**：补 set_user_token 的 T2 / T3 两侧 cargo test，验证 T3+ 同值 short-circuit、T2 同值仍 emit。

### Gap-3：`setAllowedCalls(key, [])`（空 scopes）的 revert 行为无 byte-eq 验证

**位置**：`account_keychain.rs:1122-1124` vs writer `mod.rs:476-478`

**现状**：代码层面两侧都 `return Err(invalid_call_scope())`，逻辑一致。但没有 cargo test 或 mainnet replay 验证 revert data bytes 完全一致。

**风险**：低 — 代码 mirror 关系清晰，error type 一致。

**补法**：cargo test `set_allowed_calls_empty_scopes_reverts_on_t3_and_t4`，断言 revert reason bytes。

### Gap-4：`get_allowed_calls` 对 revoked / expired key 返回 `(true, [])` 的特殊语义无测试

**位置**：`account_keychain.rs:1160-1180`

**事实**：ABI 文档约定 "Missing, revoked, or expired keys report scoped deny-all"。leafage 实现了，但没单测验证调用方按 `isScoped=true && scopes=[]` 解读为 deny-all。

**风险**：业务侧如果误解为 unrestricted（`isScoped=false`）会有安全后果，但这是消费方的错误而非 leafage。

**补法**：cargo test 构造 revoked / expired key，断言返回 `(true, [])`。

### Gap-5：RLP trailing canonical 边缘组合未穷举

**位置**：`fee_payer.rs:590-621`

**现状**：`test_key_authorization_rlp_trailing` 覆盖了 `(None,None,None)`、`(Some,None,None)`、`(Some,Some,None)`。**缺** `(None, Some, None)`、`(None, None, Some)`、`(Some, None, Some)` 等组合。

**风险**：低 — mainnet 罕见，但手写 RLP 出错最容易在边缘组合。

**补法**：补三个组合的 round-trip 单测。

### Gap-6：consistency-checker 历史窗口浅

**事实**：测试报告 §9 只看了最近 1000 行 0 inconsistency。T3 激活后 ~3.8M 块、T4 激活后 ~136k 块都没全量扫过。

**风险**：低 — 测试期间 leafage 已经追上 tip 且 1.1 17 块 root 全过，间接说明无大面积分歧。但 T3 激活附近的边界块（block 16,986,000 ± 100）未抽样。

**补法**：`docker compose logs consistency-checker | grep -c inconsistency` 全量扫；脚本里抽样 T3 activation 后前 100 块的 stateRoot。

---

## Subagent 误报（已经核实）

### 误报-1：H-1 `removeAllowedCalls` 无 T3 gate

**Subagent 判断**：leafage `remove_allowed_calls` 不像 `set_allowed_calls` 有 `is_t3()` gate；writer 行为是 revert。

**事实**：writer 同样**没有** `is_t3()` gate（`/Users/lihe/code/task_tempo/crates/precompiles/src/account_keychain/mod.rs:490-509`）。leafage 是精准 mirror。两侧 pre-T3 调用都走"检查 admin → 检查 key → 若 is_scoped=false 返回 Ok(())"。这是 writer 的设计选择（root-only removal 不需要 hardfork gate），不是 leafage 的实现 gap。

**结论**：不是 bug，不补测。

### 误报-2：M-4 `derive_scope_counts` recipients 字段没单测验证 gas

**Subagent 判断**：现有 `derive_scope_counts_aggregates_across_nested_rules` 只验聚合，没验 recipients 真的进 gas 公式。

**事实**：`crates/leafage-evm-chains/src/tempo/api/exec.rs:2065-2078` 有 `call_scope_extra_gas_with_scopes`：
```rust
let expected = BASE_SCOPE_GAS + 2 * TARGET_SCOPE_GAS + 3 * SELECTOR_SCOPE_GAS + 5 * RECIPIENT_SCOPE_GAS;
assert_eq!(call_scope_extra_gas(&s), expected);
```
明确把 `recipients=5` 乘进 gas 数值并断言。

另外 T4-fx6 mainnet replay 的 stateRoot byte-identical 也间接验证了完整 gas 公式（gas 错会算出不同 stateRoot）。

**结论**：不是 gap，不补测。

---

## 我的判断与建议

### 合 PR 前必修（2 项）
1. **Bug-1**：`verify_and_update_spending` rollover 路径合并 write（mirror writer）
2. **Bug-2**：`refund_spending_limit` 加 `if max == 0 { refunded } else { refunded.min(max) }` 三元

修复后两件事的补测：
- Bug-1 单测：构造 `now == period_end` 的 rollover，断言 SSTORE 次数 = 1
- Bug-2 单测：构造 `max == 0` (pre-T3 数据) 的 refund，断言 remaining 不被清零

### 合 PR 前可选补（提升 coverage）
- Gap-1：deny-all RLP byte-eq cargo test（30 行）
- Gap-2：set_user_token T2/T3 对比 cargo test（40 行）

### Follow-up issue（不阻塞合 PR）
- Gap-3 / Gap-4 / Gap-5：低风险测试 gap
- Gap-6：运维侧 — consistency-checker 全量扫脚本

### 元结论

Subagent 报告**不能盲信**。10 项判断里 2 项误报，约 20% 噪音。但它指出的 H-3 / M-1 两个真实 bug 是当前测试套件用 17 个采样块 + 7 笔 mainnet fixture **抓不到**的——因为它们都在精确边界条件下触发（period_end 边界 / pre-T3 key 在 T3+ refund），mainnet replay 概率极低。

这两个 bug 都出现在 FU-4 / FU-6 这两个 follow-up commit 里，**说明 follow-up 系列虽然各自有 cargo test 通过，但 cargo test 的 fixture 主要是"happy path"，没有专门覆盖 writer 自己的兼容性分支（max=0 / write ordering）**。后续做类似 hardfork mirror 类工作时，应该把 writer 的所有 `if` 分支列出来一一对照覆盖，不要默认"逻辑相同就行"。

---

## 2026-05-21 修复执行记录

按用户指示一次性修完全部 8 项。实际处理结果：

### 已修（4 项）

| 项 | 改动 | 验证 |
|---|---|---|
| **Bug-1** | `account_keychain.rs:902-942` 重写 `verify_and_update_spending`，pre-T3 早返回 + T3+ rollover 合并 write（mirror writer 1117-1180）| workspace `cargo check` + 174 cargo test pass |
| **Bug-2** | `account_keychain.rs:966-981` 加 `if state.max == 0 { refunded } else { refunded.min(state.max) }` 三元（mirror writer 1238-1247）| 同上 |
| **Gap-1** | `fee_payer.rs:1270-1303` 新增 `key_authorization_deny_all_rlp_byte_fixture` 单测：deny-all envelope 字节级 fixture（29 bytes：list_header 0xdc + chain_id + key_type + key_id + expiry + limits(0x80) + allowed_calls(0xc0)）| 单测通过 |
| **Gap-5** | `fee_payer.rs:1270-1303` 新增两个边缘 RLP 组合单测：`(None, Some([]), None)` 和 `(None, None, Some([]))` | 两单测通过 |
| **Gap-6** | 远端 `blockchain-misc-x1` 启动了 `tempo-t4-consistency` 容器（**首次审计 false PASS 修正**，见下） | 容器正在追上 leafage tip 21,150,065+，0 inconsistency |

### Gap-6 的副发现 — false PASS 修正

执行 Gap-6 时发现 `tempo-t4-consistency` 容器**从未运行过**（`docker inspect` 返回 "No such object"）。2026-05-19 测试报告 §9 "最近 1000 行 0 inconsistency PASS" 的结论错误：grep 0 匹配是因为日志为空（容器不存在），不是因为真的无 inconsistency。已 `docker compose up -d consistency-checker` 启动，容器正常拉起开始消费 kafka。Test report §9 已修正措辞。

### 未修，留 follow-up（3 项）

**根因**：leafage 缺写入型 mock storage provider（writer 有 `HashMapStorageProvider::new_with_spec` 支持完整 `verify_and_update_spending` / `refund_spending_limit` / `set_user_token` 全路径单测；leafage 只有 read-only `ReadOnlyStorageProvider`，sstore unreachable）。补 3 项需要先 mirror writer 加一个 `HashMapStorageProvider`（约 150-200 行新增基础设施），**独立 PR**。

| 项 | 卡点 | 建议 |
|---|---|---|
| **Bug-1 / Bug-2 e2e 单测** | 需写入 storage 跑全函数路径 | 独立 follow-up PR：mirror writer `HashMapStorageProvider`，然后回填两个 bug 的 e2e 单测 |
| **Gap-2** `set_user_token` T2/T3 对比 | 同上 + 需要 `is_tip20` mock 返回 true | 同上 |
| **Gap-3** `setAllowedCalls(key, [])` revert byte | 需要先通过 `ensure_admin_caller` / `load_active_key`，都依赖 storage | 同上 |
| **Gap-4** `get_allowed_calls` revoked/expired key 返回 `(true, [])` | 需要先 authorize + revoke key，依赖 storage write | 同上 |

**当前 e2e 验证替代方案**：dev 环境部署修复后的 leafage 镜像，让 consistency-checker 跑一段时间。如果 Bug-1（rollover 双写）/ Bug-2（max=0 清零）的修复改变了任何 mainnet 实际块的 state diff，consistency-checker 会立刻报 mismatch。

### 文件改动汇总（待 commit）

```
M  crates/leafage-evm-chains/src/tempo/precompile/account_keychain.rs   # Bug-1 + Bug-2
M  crates/leafage-evm-chains/src/tempo/fee_payer.rs                     # Gap-1 + Gap-5 (3 个新单测)
M  docs/test-report-tempo-t3-t4.md                                      # §9 false PASS 修正
M  docs/test-plan-tempo-t3-t4.md                                        # 之前 T4 fixture 节
?  docs/test-coverage-audit.md                                          # 本文
?  docs/test-report-tempo-t3-t4.md                                      # 之前的测试报告
```

总改动：约 80 行新增（含注释）+ 重写 1 个函数 + 改动 1 个 if 分支。所有改动通过 workspace `cargo check` + 174 unit tests 全过。
