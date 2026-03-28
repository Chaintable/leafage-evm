# estimateGas 1564 Gas 差异分析报告

## 现象

`estimateGas` 对特定地址 `0x0cac...cd20`（有 TIP-20 余额的 AA 用户），writer 返回 22414，leafage 返回 23982，差 1568 gas。其他地址（0x983b、0x0000、0xdead）两端一致。

## 测试数据

Block: `0xA00000`, Target: TIP20 PATH_USD `balanceOf(0x0cac)`

| From | Writer estimateGas | Leafage estimateGas | Diff |
|------|-------------------|--------------------|----|
| 0x0000 (nonce=0) | 276983 | 276983 | 0 |
| 0x0cac (nonce=38912, TIP20 balance=4769343) | 22414 | 23982 | 1568 |
| 0x983b (nonce=220, TIP20 balance=0) | 23982 | 23982 | 0 |
| 0xdead (nonce=0) | 276983 | 276983 | 0 |

## 精确测定最小 gas

通过 `eth_call` 二分搜索确认：

| | Writer min gas | Leafage min gas |
|---|---|---|
| from=0x0cac | **22080** | **23644** |
| Execution gas (减去 21000 base) | 1080 | 2644 |

精确差值：23644 - 22080 = **1564 gas**

## 根因分析

### 发现 1: Writer 的 caller_gas_allowance 预热了 storage slot

Writer 的 `eth_estimateGas` 流程（reth `estimate.rs`）：

```
1. db = State::builder().with_database(state).build()     // line 95
2. tx_env = self.create_txn_env(&evm_env, request, &mut db)  // line 102
3. caller_gas_allowance(&mut db, &evm_env, &tx_env)        // line 124
   → db.get_fee_token(tx, caller)
   → db.get_token_balance(fee_token, caller)               // 读 TIP20.balances[0x0cac]
4. evm = self.evm_config().evm_with_env(&mut db, evm_env)  // line 131, 同一个 db
5. evm.transact(tx_env)                                     // line 154, 二分搜索
```

`caller_gas_allowance` 在 step 3 通过 `TempoStateAccess::sload` 读了 `TIP20.balances[0x0cac]`，这个值进入了 `State` 的 cache。

### 发现 2: Leafage eth_call 不检查 nonce==0 surcharge（独立 bug）

测试发现 leafage 的 `eth_call` 从 nonce=0 地址 (0x0000) 用 gas=22406 **成功**，但 writer 正确拒绝（`intrinsic_gas=271064 > 22406`）：

```
Leafage eth_call from=0x0000 gas=22406: OK (返回结果)
Writer  eth_call from=0x0000 gas=22406: FAIL (insufficient gas for intrinsic cost)
```

这说明 leafage 的 `eth_call` 路径的 `validate_initial_tx_gas` 没有正确应用 TIP-1000 nonce==0 surcharge。这是一个独立的 bug。

### 发现 3: 差值分析

1564 gas ≈ `COLD_SLOAD_COST(2100) - WARM_STORAGE_READ_COST(100)` = 2000 gas，但不完全相等。具体分解：

- Leafage execution gas = 2644 = 预编译 sload (cold, 2100) + keccak256 (36) + input_cost (12) + 框架开销
- Writer execution gas = 1080 — 比 leafage 少约 1564，可能是 sload 在 warm 状态下执行

但 reth 的 `State` cache 和 revm 的 journal warm/cold set 是**不同层面**的。`State` cache 是 DB 读缓存，不影响 EIP-2929 gas 计费。EVM journal 在每次 `transact()` 时重置 accessed set。

**因此 warm/cold 预热假设可能不成立。** 实际差异更可能来自 leafage eth_call 路径的 nonce==0 surcharge bug — 导致两端的 `initial_gas` 扣除量不同，分配给 execution 的 gas 不同。

## 结论

1. **Leafage 有一个独立 bug**: `eth_call` 路径没有正确应用 TIP-1000 nonce==0 surcharge (250k gas)。这影响所有 nonce=0 地址的 eth_call gas 行为。需要排查为什么 `TempoHandler::validate_initial_tx_gas` 在 eth_call 路径没有生效。

2. **1564 gas 差异**的根因需要在修复 eth_call nonce bug 后重新评估 — 当前的对比数据受到这个 bug 的干扰，不能得出可靠结论。

3. **不是 writer 的 bug** — 之前怀疑 writer 的 `caller_gas_allowance` 污染 warm set 的假设需要更多证据。在修复 leafage eth_call bug 后再对比。

## 后续行动

1. 排查并修复 leafage eth_call 路径的 nonce==0 surcharge 缺失
2. 修复后重新测试 0x0cac 地址的 estimateGas 差异
3. 如果差异消失，说明根因是 eth_call bug；如果仍存在，继续排查 warm/cold 假设
