# leafage-evm Tempo T3 / T4 改造测试计划

## 上下文

本测试计划覆盖 PR `feature/tempo-t3-t4-adaptation`
（commit `25324a6` 起始）的端到端验证。改造将 leafage-evm 的 Tempo 模块
从 T2 基线升到 T4，加上 2 个新预编译（signature_verifier / address_registry）、
TIP-20 paused + virtual recipient forwarding、stablecoin_dex T4 paused、
AA key_auth_gas T3/T4 分支等。

完整 PR 描述：`feature/tempo-t3-t4-adaptation`
未做的 deferred 项清单：`docs/tempo-t3-t4-followups.md`
T2 baseline 测试计划参考：`~/code/task_tempo/docs/test-plan-generic-node.md`

T3 mainnet 激活：2026-04-27 14:00 UTC，timestamp `1777298400`，约 block 16,986,000。
T4 mainnet 激活：2026-05-18 14:00 UTC，timestamp `1779112800`。

## 测试环境

| 组件 | 镜像 / 配置 |
|---|---|
| 主机 | `blockchain-misc-x1` (ap-northeast-1a) |
| writer | `blockchain/tempo:d6e55f6` (v1.7.0, T4-ready) port 8545 |
| ETL | `background-tracer:amd64-v0.1.32` (host network, push topic `nodex_pipeline_4217_f490914c`) |
| **leafage (被测)** | `leafage-evm-x:25324a6-amd64` (host network, port 8536) |
| consistency-checker | `consistency-checkerx:amd64-v1.0.18` (host network, port 8886) |
| etcd | host systemd etcd (127.0.0.1:2379) |
| compose | `/data/tempo-t4/docker-compose.yml` |
| 对照 RPC | 1. **writer** localhost:8545 (同机权威源)<br>2. 公司 dev `blockchain-misc-x3:tempo-dev:8566`<br>3. 官方 `https://rpc.tempo.xyz` |

测试日期：待 leafage 同步到 head 后开始。

### 测试块覆盖矩阵

| ID | Block height | hex | hardfork | 特征 | 来源 |
|---|---|---|---|---|---|
| C1 | 10,100,400 | 0x9a1eb0 | T1B-T2 | pre-T3 控制组（同 T2 测试计划） | task_tempo T2 计划 |
| C2 | 16,985,999 | 0x10333cf | **T2 → T3 边界** | T3 激活前最后一块 | 计算 |
| **T3-A** | 17,074,116 | 0x10487c4 | T3 | **address_registry register 调用 + AA tx** | task_tempo T3 报告 |
| **T3-B** | 17,500,000 | 0x10b0760 | T3 | EIP-1559 + TIP-20 调用 | task_tempo T3 报告 |
| **T3-C** | 18,210,816 | 0x115e000 | T3 | AA tx (signature_type=secp256k1) | task_tempo T3 报告 |
| **T3-D** | 18,427,929 | 0x1193019 | T3 | address_registry 调用 | task_tempo T3 报告 |
| **T3-E** | 18,505,730 | 0x11a6002 | T3 | **address_registry + AA tx (webAuthn signature)** | task_tempo T3 报告 |
| **T3-F** | 19,600,000 | 0x12b1280 | T3 | 高 tx count（5 Legacy） | task_tempo T3 报告 |
| **T3-G** | ~19,940,000+ | live | T3 | 实时区块，验证 follow + Kafka 同步 | runtime |
| **T4-A** | TBD post-5/18 | TBD | **T4** | T4 激活后首块 | 待 5/18 后选 |
| **T4-B** | TBD | TBD | T4 | T4 stablecoin_dex paused 触发块（如果发生） | 待选 |
| **T4-C** | TBD | TBD | T4 | T4 AA tx with call scopes（scope_counts 完整路径） | 待选 |

---

## 1. 顶层 RPC 一致性（leafage vs writer，byte-identical）

每个测试块对每笔 tx 做以下对比，全部要求 leafage 返回 = writer 返回（同 JSON sha256）。

| # | 测试项 | 验证方法 | 期望结果 |
|---|---|---|---|
| 1.1 | `eth_getBlockByNumber(b, false)` | leafage / writer 双侧调用，对比 `hash` / `stateRoot` / `transactionsRoot` / `receiptsRoot` 4 个 root byte-identical | 9 块 × 4 root = 36/36 |
| 1.2 | `eth_getBlockByNumber(b, true)` | 对比 `transactions` 数组 sha256 | 9/9 byte-identical |
| 1.3 | `eth_getTransactionReceipt(tx)` | 对每笔 tx 对比 `status` / `gasUsed` / `cumulativeGasUsed` / `contractAddress` / `logs` | 全 byte-identical |
| 1.4 | `eth_call` 任意只读调用 | 见 §2/§3 各预编译详测 | byte-identical |
| 1.5 | `eth_estimateGas` | AA tx 和 普通 tx | 见 §5 AA gas 详测 |
| 1.6 | `eth_getCode(addr, b)` | 对所有 TIP-20 + 9 个已有预编译 + 2 个新预编译地址 | byte-identical |
| 1.7 | `eth_getStorageAt(addr, slot, b)` | 对 fee_manager / tip20 paused slot / address_registry data slot | byte-identical |

---

## 2. T3 新预编译 — `signature_verifier` (TIP-1020, 0x5165...)

### 2.1 dispatch 行为（不依赖 state）

| # | 测试项 | 调用 | 期望 |
|---|---|---|---|
| 2.1.1 | pre-T3 block 上调用 | `eth_call` to `0x5165...` on block C1 (pre-T3) | leafage 返回 = writer 返回（应该是 unknown_selector revert，因为 pre-T3 不识别为 precompile） |
| 2.1.2 | post-T3 block 短 calldata (<4 byte) | `eth_call` data=0x00 on T3-A | revert (unknown selector), leafage = writer |
| 2.1.3 | post-T3 oversized calldata | `eth_call` data 长度 > MAX_CALLDATA_LEN (2212) | revert (InvalidFormat), leafage = writer |

### 2.2 `recover(bytes32 hash, bytes signature)` 

| # | scheme | 测试构造 | 期望 |
|---|---|---|---|
| 2.2.1 | secp256k1 | 已知 EOA 私钥签 hash=0xAA*32，calldata = abi.encode(recover, hash, sig) | leafage recover address = writer recover address = 已知 EOA addr |
| 2.2.2 | P256 | 用 p256 库生成 keypair + sign | leafage = writer = derive_p256_address(x, y) |
| 2.2.3 | WebAuthn | 构造 clientDataJSON + authData + P256 sign over sha256(authData \|\| sha256(clientDataJSON)) | leafage = writer |
| 2.2.4 | high-s P256 (malleability) | 强制 s > P256N_HALF | leafage = writer = revert (InvalidSignature) |
| 2.2.5 | 错误长度 secp (64 / 66 字节) | calldata 长度错 | revert，leafage = writer |
| 2.2.6 | unknown type byte (0x05 + 129 字节) | first byte 0x05 | revert (InvalidFormat)，leafage = writer |

### 2.3 `verify(address signer, bytes32 hash, bytes signature)`

| # | 测试项 | 期望 |
|---|---|---|
| 2.3.1 | 正确 signer | 返回 true，leafage = writer |
| 2.3.2 | 错误 signer | 返回 false，leafage = writer |
| 2.3.3 | 无效签名 | revert (InvalidSignature)，leafage = writer |

### 2.4 gas 扣费验证

| # | scheme | 期望 gas | 验证 |
|---|---|---|---|
| 2.4.1 | secp256k1 | 3,000 + input_cost | `eth_estimateGas` to 0x5165 with valid secp sig，leafage = writer |
| 2.4.2 | P256 | 8,000 + input_cost | 同上 P256 |
| 2.4.3 | WebAuthn | 8,000 + input_cost + webauthn data 字节 cost | 同上 WebAuthn |

---

## 3. T3 新预编译 — `address_registry` (TIP-1022, 0xFDC0...)

### 3.1 dispatch + pre-T3 gate

| # | 测试项 | 期望 |
|---|---|---|
| 3.1.1 | pre-T3 block 调用 0xFDC0... | revert (unknown_selector)，leafage = writer |
| 3.1.2 | post-T3 block 短 calldata | revert，leafage = writer |

### 3.2 view 方法对照（read-only，依赖 state）

| # | 方法 | 测试块 | 期望 |
|---|---|---|---|
| 3.2.1 | `getMaster(bytes4 masterId)` | T3-A 上对已注册 masterId 调用 | 返回正确 master_address，leafage = writer |
| 3.2.2 | `getMaster(未注册 masterId)` | T3-A | 返回 `Address::ZERO`，leafage = writer |
| 3.2.3 | `resolveRecipient(eoa)` | T3-A non-virtual addr | 返回原 addr，leafage = writer |
| 3.2.4 | `resolveRecipient(virtual_addr_unregistered)` | T3-A | revert (VirtualAddressUnregistered)，leafage = writer |
| 3.2.5 | `resolveRecipient(virtual_addr_registered)` | T3-A | 返回 master_address，leafage = writer |
| 3.2.6 | `resolveVirtualAddress(addr)` | T3-A 各种地址类型 | 返回 master 或 zero，leafage = writer |
| 3.2.7 | `isVirtualAddress(addr)` pure | 不依赖 state | leafage = writer |
| 3.2.8 | `decodeVirtualAddress(addr)` pure | 不依赖 state | 返回 (isVirtual, masterId, userTag)，leafage = writer |

### 3.3 register 写入路径（在 `eth_call` 模拟中）

| # | 测试项 | 期望 |
|---|---|---|
| 3.3.1 | `registerVirtualMaster(salt)` 已注册 master | 32-bit PoW pass，emit `MasterRegistered`，return masterId — leafage 模拟 result = writer 模拟 result |
| 3.3.2 | `registerVirtualMaster(salt)` PoW fail | revert (ProofOfWorkFailed) |
| 3.3.3 | `registerVirtualMaster(salt)` from virtual address | revert (InvalidMasterAddress) |
| 3.3.4 | `registerVirtualMaster(salt)` from TIP-20 address | revert (InvalidMasterAddress) |
| 3.3.5 | `registerVirtualMaster(salt)` duplicate masterId | revert (MasterIdCollision) |

### 3.4 event 捕获

| # | 测试项 | 期望 |
|---|---|---|
| 3.4.1 | T3-A receipt logs 中 `MasterRegistered` event | leafage `eth_getLogs(filter=address_registry)` 返回数量 = writer = task_tempo T3 报告 R1 中提到的 8 个 (in 17M-18.6M 窗口) |

---

## 4. TIP-20 T3 行为（virtual recipient forwarding + paused mint/burn + reward virtual rejection）

### 4.1 virtual recipient forwarding

| # | 测试项 | 测试块 | 期望 |
|---|---|---|---|
| 4.1.1 | `balanceOf(virtual_addr)` | T3-A/D/E post-T3 | 返回 master 的 balance，leafage = writer |
| 4.1.2 | `balanceOf(master_addr)` 对应 | 同上 | 返回 credited balance，leafage = writer |
| 4.1.3 | `eth_call transfer(eoa, virtual_addr, amount)` 模拟 | post-T3 | leafage 模拟 result = writer 模拟 result（state diff 中 master balance 增加） |
| 4.1.4 | `transfer to unregistered virtual` | post-T3 | revert (VirtualAddressUnregistered)，leafage = writer |
| 4.1.5 | `transfer to virtual` on pre-T3 | C1 | 字面 virtual balance 增加（旧行为），leafage = writer |

### 4.2 paused mint / burn (TIP-1038 #2)

| # | 测试项 | 测试块 | 期望 |
|---|---|---|---|
| 4.2.1 | post-T3 `mint` on paused token | 找一个 paused TIP-20 token (如果链上有，或构造测试) | revert (ContractPaused)，leafage = writer |
| 4.2.2 | post-T3 `burn` on paused token | 同上 | revert，leafage = writer |
| 4.2.3 | post-T3 `burn_blocked` on paused token | 同上 | revert，leafage = writer |
| 4.2.4 | pre-T3 `mint` on paused token | C1 | 旧行为允许（不 revert），leafage = writer |

### 4.3 rewards virtual rejection

| # | 测试项 | 期望 |
|---|---|---|
| 4.3.1 | post-T3 `setRewardRecipient(virtual_addr)` | revert (InvalidRecipient)，leafage = writer |
| 4.3.2 | post-T3 `setRewardRecipient(eoa)` | success，leafage = writer |
| 4.3.3 | pre-T3 `setRewardRecipient(virtual_addr)` | 允许（旧行为），leafage = writer |

### 4.4 已有 transfer paused 兼容性（不应被打破）

| # | 测试项 | 期望 |
|---|---|---|
| 4.4.1 | post-T3 `transfer` on paused token | revert (ContractPaused)，跟 T2 一致行为，leafage = writer |

---

## 5. AA `key_auth_gas` 四分支公式（含 partial scope-driven）

### 5.1 不带 call scope 的 AA tx（byte-equivalent 路径）

| # | hardfork | 测试块 | 期望 |
|---|---|---|---|
| 5.1.1 | T1B | block 中 AA tx with limits + Secp sig | `eth_estimateGas` leafage = writer (byte-exact) |
| 5.1.2 | T2 | 同上但 timestamp > T2 | leafage = writer |
| 5.1.3 | **T3** | T3-C (AA tx secp) | leafage = writer (limit_slots × 2 = `num_limits * 2 * sstore_cost` 起作用) |
| 5.1.4 | **T3 with P256** | 找/构造 P256 AA tx on post-T3 | leafage = writer (含 P256_VERIFY_GAS) |
| 5.1.5 | **T3 with WebAuthn** | T3-E (webAuthn AA tx) | leafage = writer (含 WebAuthn calldata cost) |
| 5.1.6 | **T4** | T4-A post-5/18 AA tx no scopes | leafage = writer + BASE_SCOPE_GAS (5_000) |

### 5.2 带 call scope 的 AA tx

> FU-2 完成（commit `1d56765`）后 ScopeCounts 已从 envelope 解析填充，本节
> 期望 leafage = writer 字节相同。

| # | 测试项 | 期望 |
|---|---|---|
| 5.2.1 | T4 AA tx with `allowedCalls=[]` | leafage = writer (has_allowed_calls=true，BASE_SCOPE_GAS + scope_slots=1) |
| 5.2.2 | T4 AA tx with 1 scope/1 selector/1 recipient | leafage = writer (含 TARGET+SELECTOR+RECIPIENT extra gas) |
| 5.2.3 | T3 AA tx with allowedCalls | leafage = writer (T3 storage_slots 公式) |
| 5.2.4 | T4 AA tx with 0 limits 0 scopes | leafage = writer (BASE_SCOPE_GAS only) |

### 5.3 单元测试（cargo test）

| # | 测试 | 验证 |
|---|---|---|
| 5.3.1 | `key_auth_gas_pre_t1b_uses_heuristic` | 4 fork branches 公式 byte-exact vs hand-computed |
| 5.3.2 | `key_auth_gas_t3_doubles_limit_slots` | T3 limit×2 |
| 5.3.3 | `key_auth_gas_t4_adds_base_scope_gas` | T4 + BASE 5k |
| 5.3.4 | `call_scope_storage_slots_{none, empty, t3, t4}` | helper 公式与 writer 对齐 |
| 5.3.5 | `call_scope_extra_gas_with_scopes` | TARGET=7k + SELECTOR=7k + RECIPIENT=5k 公式 |
| 5.3.6 | `key_auth_gas_t4_with_scope_counts` | 端到端 T4 含 scope_counts |

> 已经在 PR 中：`cargo test -p leafage-evm-chains tempo::api::exec::tests::key_auth_gas` 13 全 PASS。

---

## 6. Stablecoin DEX T4 paused (TIP-1046)

需要 **T4 mainnet 激活后 (5/18)** 才能跑链上对照。

| # | 测试项 | 测试块 | 期望 |
|---|---|---|---|
| 6.1 | post-T4 `placeOrder` non_escrow_token paused | T4-B | revert (ContractPaused)，leafage = writer |
| 6.2 | post-T4 `placeFlipOrder` non_escrow paused | T4-B | revert，leafage = writer |
| 6.3 | post-T4 `placeFlipOrder` internal_balance_only escrow paused | T4-B | revert，leafage = writer |
| 6.4 | pre-T4 同样调用 paused token | C1/T3-A | 旧行为通过（不 revert），leafage = writer |
| 6.5 | post-T4 non-paused token swap | T4-A | success，leafage = writer (state diff byte-identical) |

---

## 7. hardfork 路由 (timestamp → hardfork)

每个测试块抽样验证 leafage 内部用了正确 hardfork。

| # | block timestamp | leafage `eth_chainId` / behavior 验证 | 期望 hardfork |
|---|---|---|---|
| 7.1 | C1 block timestamp ≈ T1B-T2 范围 | leafage gas 估算用 T2 公式 | T2 |
| 7.2 | T3-A timestamp = 1,777,298,400 + ~hours | T3 行为生效（virtual forwarding, paused mint） | T3 |
| 7.3 | T4-A timestamp >= 1,779,112,800 | T4 行为生效 (stablecoin paused, BASE_SCOPE_GAS) | T4 |
| 7.4 | T2/T3 边界 block 16,985,999 / 16,986,000 | 后者起 T3 行为 | 边界正确 |

cargo test 覆盖：
- `from_timestamp_t3_activated` / `from_timestamp_t4_activated` 边界（已在 PR）
- `is_methods_on_t4` / `is_methods_on_t3` 单调性（已在 PR）

---

## 8. CallScope 行为（FU-1 / FU-5 已 land）

> FU-1 (commit `7706744`) wire 了三层 CallScope storage 读写 + ABI rename 到
> writer 对齐的 `setAllowedCalls` / `getAllowedCalls` / `removeAllowedCalls`。
> FU-5 (commit `8a93c60`) 加了 T3/T4 validate 分支。

| # | 测试项 | 期望 |
|---|---|---|
| 8.1 | post-T3 `eth_call setAllowedCalls(...)` 合法 scope | success；写 storage 字节与 writer 一致 |
| 8.2 | post-T3 `eth_call getAllowedCalls(account, keyId)` 对已配置 scope 的 account | 返回 `(isScoped=true, scopes)` 与 writer state diff 字节一致 |
| 8.3 | post-T3 `eth_call removeAllowedCalls(keyId, target)` | success；后续 getAllowedCalls 不再包含该 target |
| 8.4 | pre-T3 `setAllowedCalls` 调用 | revert (InvalidCallScope)；同 writer |
| 8.5 | T3 `setAllowedCalls` target = 未部署 TIP-20 prefix 地址 | revert (InvalidCallScope) (stateful TIP20Factory 拒绝) |
| 8.6 | T4 `setAllowedCalls` target = 未部署 TIP-20 prefix 地址 | success (stateless 仅 prefix 检查) |
| 8.7 | account_keychain storage layout slot 4 (`key_scopes`) 与 writer 字节一致 | 通过 `eth_getStorageAt` 对比 leafage = writer |

---

## 9. consistency-checker（自动状态完整性）

`tempo-t4-consistency` 容器后台跑，对每个新 block 触发 leafage 跟 Kafka 内部数据 byte-identical 校验。

| # | 测试项 | 验证 |
|---|---|---|
| 9.1 | leafage 启动到 head 期间 0 inconsistency | `sudo docker compose logs consistency-checker | grep -i "inconsistency\|mismatch"` 返回空 |
| 9.2 | 跨 T3 边界 block 16,985,999 → 16,986,000 一致 | consistency-checker 不报错 |
| 9.3 | 跨 T4 边界（5/18 后）一致 | consistency-checker 不报错 |
| 9.4 | 24 小时 burn-in，无 panic | leafage container 状态 Up 24h 无 restart |

---

## 10. 性能 / 长跑回归

| # | 测试项 | 测试块范围 | 期望 |
|---|---|---|---|
| 10.1 | 200 块批量 RPC byte-identical | post-T3 连续 200 块 (e.g. 19,500,000-19,500,199) | 200/200 PASS (跟 T2 测试计划 §13 一致) |
| 10.2 | `eth_call` 平均延迟 | 200 次 `balanceOf` 调用 | leafage / writer ratio < 2x |
| 10.3 | 跨进程内存 stability | leafage 24h 内存 < 8GB（视 nodex 数据规模） | RSS 不持续增长 |
| 10.4 | live block apply 延迟 | 每个新 block 从 writer 写入到 leafage state 推进 | < 2 秒 |

---

## 11. 已知差异 / 不计 FAIL 的项

| # | 差异 | 原因 |
|---|---|---|
| 11.1 | ~~AA tx with call scopes 的 `eth_estimateGas` 偏低~~ | ✅ FU-2 完成 (commit `1d56765`)，scope_counts 从 envelope 解析填充 |
| 11.2 | ~~`getCallScope` 返回 empty~~ | ✅ FU-1 完成 (commit `7706744`)，ABI 已 rename 为 `getAllowedCalls`，三层读路径已 wire |
| 11.3 | ~~`setCallScopes` revert~~ | ✅ FU-1 完成；ABI 已 rename 为 `setAllowedCalls`，写路径已 wire |
| 11.4 | ~~spending limit 周期性 reset 未实现~~ | ✅ FU-3 / FU-4 / FU-6 全部完成 (commits `59a2e44` / `0ac5f8e` / `b850804`) |
| 11.5 | `consensus_context` header 字段缺失 | 设计 non-goal，待 FU-7 业务方需求 |
| 11.6 | TIP-1016 state gas 未实现 | mainnet flag 未启用 (FU-11) |

---

## 12. 执行流程

```bash
# 0. 在 blockchain-misc-x1 上，部署见 /data/tempo-t4/docker-compose.yml
# 1. 等 writer 同步到 head + leafage 同步到 head
ssh blockchain-misc-x1 'sudo docker compose -f /data/tempo-t4/docker-compose.yml logs leafage | tail -1'

# 2. 跑 byte-identical 对比脚本
#    参考 task_tempo/docs/test-plan-generic-node.md 的 /tmp/cmp_block.sh，
#    把 "official RPC vs writer" 改成 "writer vs leafage"
ssh blockchain-misc-x1 'bash /tmp/cmp_leafage_block.sh 17074116'   # T3-A
ssh blockchain-misc-x1 'bash /tmp/cmp_leafage_block.sh 18505730'   # T3-E (webAuthn)

# 3. 跑批量回归
ssh blockchain-misc-x1 'for h in $(seq 19500000 19500199); do bash /tmp/cmp_leafage_block.sh $h; done | tee /tmp/regression.log'
grep -c FAIL /tmp/regression.log

# 4. 跑预编译特化测试（构造 calldata 直接 eth_call）
ssh blockchain-misc-x1 'bash /tmp/test_signature_verifier.sh'
ssh blockchain-misc-x1 'bash /tmp/test_address_registry.sh'

# 5. T4 激活后（5/18 14:00 UTC）：
ssh blockchain-misc-x1 'bash /tmp/test_t4_stablecoin_paused.sh'
ssh blockchain-misc-x1 'bash /tmp/test_t4_call_scope_gas.sh'

# 6. 持续监控 consistency-checker
ssh blockchain-misc-x1 'sudo docker compose -f /data/tempo-t4/docker-compose.yml logs --since 24h consistency-checker | grep -E "inconsistency|mismatch|panic"'
```

---

## 13. 测试结果汇总模板

测试完毕后，结果填入 `docs/test-report-tempo-t3-t4.md`（仿照 task_tempo 的
`test-report-v1.7.0-post-t3.md` 风格）。

| 大类 | 测试点 | 通过 | 失败 | 不适用 |
|---|---|---|---|---|
| 1. RPC 一致性 | TBD | TBD | TBD | - |
| 2. signature_verifier | 12+ | TBD | TBD | - |
| 3. address_registry | 13+ | TBD | TBD | - |
| 4. TIP-20 T3 | 10+ | TBD | TBD | - |
| 5. AA gas | 10+ | TBD | TBD | 5.2 full (FU-2 ✅) |
| 6. stablecoin_dex T4 | 5 | TBD | TBD | 5/18 后 |
| 7. hardfork routing | 4 | TBD | TBD | - |
| 8. CallScope | 7 | TBD | TBD | full byte-equivalence (FU-1 / FU-5 ✅) |
| 9. consistency-checker | 4 | TBD | TBD | T4 项待 5/18 后 |
| 10. 性能 / 长跑 | 4 | TBD | TBD | - |
| **合计** | **75+** | **TBD** | **TBD** | **TBD** |
