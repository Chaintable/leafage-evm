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
| **C3** | 20,636,963 | 0x13ad5e3 | **T3 → T4 边界** | T4 激活前最后一块 (ts=1779112799) | binary search |
| **T4-A** | 20,636,964 | 0x13ad5e4 | **T4** | T4 激活后首块 (ts=1779112800) | binary search |
| **T4-B** | 20,670,000 | 0x13b56b0 | T4 | T4+33k 块，stablecoin_dex 行为采样 | mid-T4 |
| **T4-C** | 20,700,000 | 0x13bcb40 | T4 | T4+63k 块，AA tx scope_counts 采样 | mid-T4 |
| **T4-D** | 20,720,000 | 0x13c1990 | T4 | 临近 tip，validate_call_scopes T4 stateless 采样 | late-T4 |
| **T4-live** | live tip | live | T4 | 实时块，hardfork 路由 + follow 验证 | runtime |

### T4 区间 fixture tx（mainnet 真实 tx，2026-05-19 writer 扫描）

下表 7 笔 fixture 来自 writer (port 8545) 在 T4 区间（20,636,964 ~ 20,772,759，135,795 blocks）的实际扫描结果，作为 §1/§5/§6/§8/§14 字节回归测试的具体输入。

| Fixture | Block | Tx Hash | tx type | to | 特征 | 关联 FU / §  |
|---|---|---|---|---|---|---|
| T4-fx1 | 20,636,964 (T4-A) | `0x4ccd243c79dbd256353a7abd9b26ed28814c218b930bc6616a36b7dd5b815b6e` | EIP-1559 (0x2) | stablecoin_dex `0xdec0...` | T4 首块第 1 笔，selector `0xf8856c0f`，触发 `OrderFilled` | §1.1-1.3 / §6.s / §14.1 |
| T4-fx2 | 20,637,200 | `0xe4f7cb50ee98c4ddf803ca42249d4a995fbaa07a3c8c5714ee32b53d6bd0f88e` | Tempo AA (0x76) | TIP-20 virtual `0x20c0...b950` | AccessKeySpend（legacy spend，`keyAuthorization=null`） | §5.1 / §14.3.1 |
| T4-fx3 | 20,637,236 | `0x38da83059f970a09968d50579ff20aa73765c0a796b5cdc80418b42f0d0932fa` | Tempo AA (0x76) | TIP-20 virtual | KeyAuthorized：limits=1 (`period=0`，非周期), `allowedCalls=null`, sig=webAuthn | FU-3 (period=0 baseline) |
| T4-fx4 | 20,641,039 | `0x3ac71a0cd5f9bd024106a4022ee69c9a6f12e7e8fd600c25c18b1ac87ce2bd94` | Legacy (0x0) | stablecoin_dex | placeOrder（selector `0x63813125`），触发 `OrderPlaced` | §6 |
| T4-fx5 | 20,641,109 | `0xd8e594c07dbd7cf1cefbb940d5545064f3593076a8e4e579feef67c2fdde6386` | Legacy (0x0) | stablecoin_dex | `OrderCancelled` | §6 |
| **T4-fx6 ★** | **20,675,920** | `0x06616b5ee5125ead4b653ecccd077429084b8c42783f1e6acc8378003f3cbca2` | Tempo AA (0x76) | TIP-20 virtual | **金 fixture** — keyAuthorization 同时含：① `limits=1` (token=`0x20c0...b950`, limit=10M, **period=86400** 周期 limit)；② `allowedCalls=1 scope` (target=TIP-20 prefix, **selectors=2** [`transfer 0xa9059cbb` + `0x95777d59`])；sig=webAuthn | **FU-2/3/5/6** |
| T4-fx7 | 20,700,761 | `0xbcc1da0aeb40ff031e56d9aa0bc0470f9c9405184e868f3eb90a9e7ad734527f` | Tempo AA (0x76) | TIP-20 virtual | `KeyRevoked` | §5 / §8 |

**T4 区间 event 统计**（writer eth_getLogs 双段扫描）：

| precompile / event | count | 备注 |
|---|---|---|
| account_keychain `AccessKeySpend` | ~2,434 | AA tx 触发的 spend log |
| account_keychain `KeyAuthorized` | ~129 | 其中 **10 笔 `allowedCalls` 非 null**（T4-fx6 是最早） |
| account_keychain `KeyRevoked` | 2 | |
| stablecoin_dex `OrderFilled` | ~1,049 | |
| stablecoin_dex `OrderPlaced` | ~436 | |
| stablecoin_dex `OrderCancelled` | ~428 | |
| address_registry, signature_verifier | 0 log | 这两个 precompile 不 emit event；要靠 `to` 字段筛 tx，T3 历史块更多 fixture（见 T3-A/D/E） |

> **§6.2 paused token gap**：writer 扫描中未发现 mainnet 已有 paused TIP-20 contract（OrderPlaced/OrderFilled 全部成功，无 paused 触发的 revert）。§6.2 路径需要 staging 构造或等 mainnet 实际触发；§6.s smoke 测试不受影响照常跑。

---

## 1. 顶层 RPC 一致性（leafage vs writer，byte-identical）

每个测试块对每笔 tx 做以下对比，全部要求 leafage 返回 = writer 返回（同 JSON sha256）。
T4 区间 fixture 见前文 "T4 区间 fixture tx" 节（7 笔 mainnet 真实 tx，pin 在 T4-fx1..T4-fx7）。

| # | 测试项 | 验证方法 | 期望结果 |
|---|---|---|---|
| 1.1 | `eth_getBlockByNumber(b, false)` | leafage / writer 双侧调用，对比 `hash` / `stateRoot` / `transactionsRoot` / `receiptsRoot` 4 个 root byte-identical | 15 块 × 4 root = 60/60（含 T4 sample + fixture blocks） |
| 1.2 | `eth_getBlockByNumber(b, true)` | 对比 `transactions` 数组 sha256（含 T4-fx1..fx7 7 笔 fixture 所在块） | 15/15 byte-identical |
| 1.3 | `eth_getTransactionReceipt(tx)` | 对每笔 tx 对比 `status` / `gasUsed` / `cumulativeGasUsed` / `contractAddress` / `logs`；T4-fx1..fx7 必须 byte-identical | 全 byte-identical |
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

T4 已于 **2026-05-18 14:00 UTC** 激活（block 20,636,964）。FU 之 stablecoin_dex
T4 paused gate 在 internal balance 操作路径上调用
`check_token_not_paused(token)`；该 token paused 状态来自 TIP-20 paused slot。

**precompile 地址**：`STABLECOIN_DEX_ADDRESS = 0xdec0000000000000000000000000000000000000`

### 6.1 调度层 smoke tests（不需要特定 paused token）

| # | 测试项 | 测试块 | 期望 |
|---|---|---|---|
| 6.s1 | `eth_getCode(0xdec0..., T4 block)` | T4-A | leafage = writer（precompile bytecode 0xef） |
| 6.s2 | `eth_call(0xdec0..., data=0x00, T4 block)` | T4-A | leafage 与 writer 同样 revert (unknown_selector) |
| 6.s3 | `eth_getStorageAt(0xdec0..., 0x0, T4 block)` | T4-A | 任意 slot，leafage = writer (storage layout consistency) |
| 6.s4 | dispatch on **pre-T4** block 同 calldata | C3 (T3 last) | T3 行为：调用成功或同样 revert，leafage = writer（确认 T4 gate 触发点） |
| 6.s5 | replay **T4-fx1** tx receipt + state diff | 20,636,964 | T4 首块 OrderFilled tx，leafage = writer (status / logs / gasUsed) |
| 6.s6 | replay **T4-fx4** tx (`OrderPlaced`) | 20,641,039 | leafage = writer (selector `0x63813125`) |
| 6.s7 | replay **T4-fx5** tx (`OrderCancelled`) | 20,641,109 | leafage = writer |

### 6.2 paused token 触发路径（需要特定 paused token 地址）

> **gap**: writer 在 T4 区间 (20,636,964..20,772,759, ~136k blocks) 的 stablecoin_dex
> log 扫描中未发现任何 `<PAUSED_TOKEN>` 触发的 revert tx（1049 OrderFilled + 436
> OrderPlaced + 428 OrderCancelled 全部成功）。说明 mainnet 上目前没有已 paused
> 的 TIP-20 在 stablecoin_dex 内交易。本节路径需要 staging 构造或等 mainnet 实际触发。

`<PAUSED_TOKEN>` 占位，业务侧提供或链上扫描（找一个 `tip20.paused() == true` 的 TIP-20）。
各 placeOrder/placeFlipOrder 的 selector：
- `placeOrder(...)`: `0x63813125`（已从 T4-fx4 反查 mainnet 验证）
- `placeFlipOrder(...)`: TBD（需读 stablecoin_dex.rs 取最新 sol! 签名）

| # | 测试项 | 测试块 | 期望 |
|---|---|---|---|
| 6.1 | post-T4 `placeOrder` `<PAUSED_TOKEN>` (non-escrow) | T4-B | revert (TokenPaused / ContractPaused)，leafage = writer |
| 6.2 | post-T4 `placeFlipOrder` `<PAUSED_TOKEN>` (non-escrow) | T4-B | revert，leafage = writer |
| 6.3 | post-T4 `placeFlipOrder` internal_balance_only escrow `<PAUSED_TOKEN>` | T4-B | revert，leafage = writer |
| 6.4 | pre-T4 同样调用 `<PAUSED_TOKEN>` | C1/T3-A | 旧行为通过（不 revert），leafage = writer |
| 6.5 | post-T4 non-paused `<PATH_USD>` (0x20C0...) swap | T4-A | success，leafage = writer (state diff byte-identical) |

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
| 11.7 | RPC error envelope 格式不同 | writer `{code:3, message:"execution reverted", data:"0x..."}` vs leafage `{code:-32603, message:"Reverted: \"…\""}` — 双侧 EVM 都 revert，仅 JSON-RPC 错误外壳差异。Diff 时需先 normalize (取 `.error.message ~ /reverted/i` 作 boolean) 再比对 |
| 11.8 | `eth_getCode(signature_verifier/0x5165)` 返回不同 | writer `0xef`（pre-execution change 注入），leafage `0x`（未实现 T3 新 precompile bytecode 注入）。不影响 dispatch（走 chains 层，无需真 bytecode）。同样 gap：address_registry 在未被 __initialize 触发前也是 `0x`。需要单独 followup 扩展 `Vcv2CodeInjector` 到 T3+ precompile |
| 11.9 | 不支持 `eth_getBlockReceipts` / `eth_getTransactionByHash` / `eth_getTransactionReceipt` / `eth_getLogs` | leafage 是 **state RPC node**，不是 archive。tx-history 类查询超出设计范围 |
| 11.10 | `eth_getBlockByNumber` 缺少 writer 专有字段 | `mainBlockGeneralGasLimit` / `sharedGasLimit` / `size` / `timestampMillis` / `timestampMillisPart` / `withdrawals` — leafage block env 用 revm 标准结构未携带。byte-equivalence 比对只看 4 root + transactions 数组 |

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
| 1. RPC 一致性 | 4 roots × N blocks + getCode + getStorageAt | TBD | TBD | - |
| 2. signature_verifier | 12+ | TBD | TBD | - |
| 3. address_registry | 13+ | TBD | TBD | - |
| 4. TIP-20 T3 | 10+ | TBD | TBD | - |
| 5. AA gas | 10+ | TBD | TBD | 5.2 full (FU-2 ✅) |
| 6. stablecoin_dex T4 | 7 smoke + 5 paused | TBD | TBD | 6.s1-s7 fixture replay; 6.1-6.5 paused 路径需 staging 触发（mainnet 暂无 paused token） |
| 7. hardfork routing | 4 + 边界(C3/T4-A) | TBD | TBD | - |
| 8. CallScope | 7 | TBD | TBD | full byte-equivalence (FU-1 / FU-5 ✅) |
| 9. consistency-checker | 4 | TBD | TBD | x1 上 checker 当前 stuck，需独立修复 |
| 10. 性能 / 长跑 | 4 | TBD | TBD | - |
| 14. T4 专项 | 见 §14 (14.1×3 + 14.3×7 + 14.4×5 + 14.5×3) | TBD | TBD | T4 已激活 (2026-05-18 14:00 UTC)，含 T4-fx1..fx7 mainnet fixture replay |
| **合计** | **95+** | **TBD** | **TBD** | **TBD** |

---

## 14. T4 专项测试（T4 激活后必跑）

T4 mainnet 激活 **2026-05-18 14:00 UTC** (block **20,636,964**)。FU 之 T4 改动包括：
- `stablecoin_dex` internal balance 路径 `check_token_not_paused` gate（已在 §6）
- `key_auth_gas` T4 分支：`BASE_SCOPE_GAS` + scope-driven extra gas（已在 §5.1.6 / §5.2）
- `validate_call_scopes` T4 stateless `target.is_tip20()` 分支（已在 §8.6）
- `is_t4()` hardfork 路由（已在 §7.3）

### 14.1 T4 hardfork 边界正确性

| # | 测试项 | 验证 |
|---|---|---|
| 14.1.1 | block 20,636,963 (C3, last T3): leafage 用 T3 公式 | `eth_estimateGas` AA tx no scopes leafage = writer T3 行为 |
| 14.1.2 | block 20,636,964 (T4-A, first T4): leafage 用 T4 公式，对照 **T4-fx1** | `eth_estimateGas` AA tx no scopes leafage = writer T4 行为（+BASE_SCOPE_GAS） |
| 14.1.3 | 4 个 T4 sample blocks state root + replay **T4-fx1** (块 20636964) | hash / stateRoot / transactionsRoot / receiptsRoot 全 match |

### 14.2 T4 stablecoin_dex 行为对照

见 §6（已展开 7 smoke + 5 paused 详细项）。smoke tests 不需要 paused token 即可跑，
对照 **T4-fx1 / T4-fx4 / T4-fx5** 三笔真实 mainnet tx 跑 receipt + state diff。

### 14.3 T4 AA gas 公式（key_auth_gas + scope_counts）

下表 14.3.1 / 14.3.5 / 14.3.6 用 **mainnet 真实 fixture** 跑（无需自构造 calldata）；
14.3.2 / 14.3.3 / 14.3.4 仍需自构造（mainnet 暂无对应 pattern）。

| # | 测试构造 | 测试 fixture | 期望 |
|---|---|---|---|
| 14.3.1 | AA tx with `keyAuthorization=null`（已 authorized key 复用） | **T4-fx2** (block 20,637,200) | leafage = writer; 走 stored key 路径，key_auth_gas=0 |
| 14.3.2 | AA tx with `allowedCalls=[]` (Some(vec![]), deny-all) | 自构造 on T4-A | leafage = writer; has_allowed_calls=true, scope_slots=1, BASE_SCOPE_GAS=5000 |
| 14.3.3 | AA tx with 1 scope + 1 selector + 1 recipient | 自构造 on T4-C | leafage = writer; 含 TARGET (7k) + SELECTOR (7k) + RECIPIENT (5k) extra |
| 14.3.4 | AA tx with 3 scopes + multiple selectors/recipients | 自构造 on T4-D | leafage = writer; aggregate counts 正确派生 |
| 14.3.5 | AA tx with `keyAuthorization` 含 limit (period=0, 非周期) | **T4-fx3** (block 20,637,236) | leafage = writer; FU-3 SpendingLimitState `period=0` 路径 |
| 14.3.6 | AA tx with **periodic limit (period=86400) + 1 scope/2 selectors** | **T4-fx6** (block 20,675,920) | leafage = writer; **FU-2/3/6 全集**；key_auth_gas T4 含 BASE(5k) + TARGET(7k) + SELECTOR(7k×2) = 26k extra |
| 14.3.7 | AA tx KeyRevoked | **T4-fx7** (block 20,700,761) | leafage = writer; revoke 后 key 不可复用 |

实际跑法：通过 `debug_replayTransaction` / `eth_getTransactionReceipt` 同时向 writer / leafage 发送相同 hash，对比 result（fixture 路径）；或通过 `eth_estimateGas` 自构造 calldata（14.3.2-4 路径）。FU-2 完成后 `derive_scope_counts` 已 wire，所以 leafage 不再低估。

### 14.4 T4 `validate_call_scopes` stateless（FU-5）

| # | 测试构造 | 测试块 / fixture | 期望 |
|---|---|---|---|
| 14.4.1 | `setAllowedCalls` target = TIP-20 prefix 地址 (e.g. 0x20C0…0042) **未部署** | T3-A | revert (InvalidCallScope) on both — stateful TIP20Factory 拒绝 |
| 14.4.2 | 同 14.4.1 但在 T4-A | T4-A | success on both — stateless 仅 prefix 检查 |
| 14.4.3 | `setAllowedCalls` target = 非 TIP-20 prefix (e.g. EOA) | T4-A | revert (InvalidCallScope) on both — 即使 stateless 也拒绝错前缀 |
| 14.4.4 | `setAllowedCalls` target = 已部署 TIP-20 (e.g. 0x20C0…0000 PATH USD) | T3-A 和 T4-A | success on both |
| 14.4.5 | replay **T4-fx6** keyAuthorization 中的 `allowedCalls[0].target = 0x20c0...b950` | block 20,675,920 | leafage = writer; FU-5 stateless 路径在 mainnet tx 上字节正确 |

### 14.5 T4 follow-up checklist（FU-11 等）

| # | 检查项 | 触发条件 |
|---|---|---|
| 14.5.1 | TIP-1016 state gas | writer `enable_amsterdam_eip8037` flag flip → FU-11 立刻补 |
| 14.5.2 | T4 storage_slots 公式 vs T3 | call_scope_storage_slots cargo test 已覆盖；live RPC 通过 14.3.x 验证 |
| 14.5.3 | TIP20 paused 字段 read-equivalence at T4 | 对几个已知 TIP-20 跑 `eth_call paused()` writer vs leafage |
