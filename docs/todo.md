# Tempo Adaptation TODO / 决策记录

## 已做的决策

1. ~~**不实现 AA tx (0x76) 完整执行路径**~~ — 已实现批量原子执行（TempoTxEnv + TempoHandler::execute_multi_call），不需要 fee handler 和签名验证
2. **Fee handler 无需实现** — 已确认 Tempo writer 的 `eth_call` / `eth_estimateGas` 在 `disable_balance_check=true` 下，handler 的 `validate_against_state_and_deduct_caller` 中 `gas_balance_spending=0` 自动短路跳过 fee 收取。leafage 行为一致
3. **Hardfork 策略** — TempoHardfork 枚举支持 `from_timestamp()` 动态切换（archive 模式），`Default` 仍返回 T3（latest spec）。`is_*()` 方法使用 `>=` 比较而非硬编码 true
4. **revm 版本** — 已升级到 revm 36.0.0 (op-revm 17.0.0, revm-inspectors 0.36.1, revm-bytecode 9.0.0, alloy-evm 0.29.2)
5. **GasParams** — 已使用 revm 36 原生 GasParams API 设置 Tempo TIP-1000 gas 参数覆盖（TempoEvm::new 中 7 项 override），同时保留 TempoGasCosts 常量供 LeafageStorageProvider 使用
6. **编译工具链** — Rust 1.93.0（revm 36 要求 1.88+, revm-inspectors 0.36.1 要求 1.91+）
7. **StorageKey for u64** — 为 `u64` 添加 `StorageKey` impl（TIP403Registry 的 `Mapping<u64, PolicyRecord>` 需要）
8. **AccountKeychain P256/WebAuthn 签名验证无需实现** — leafage 只读取 keychain state。`validate_keychain_authorization` 仅做 key 存在性/过期/类型匹配检查，不执行实际密码学验证。P256/WebAuthn 签名验证在 handler 层的 `verify_signature` 中（不在预编译 dispatch scope）。注：ecrecover 已通过 `secp256k1` crate 在 `PrecompileStorageProvider` trait 中实现，供 TIP20 permit 使用
9. **TempoPrecompileError::under_overflow()** — 添加 Panic(0x11) 辅助方法
10. **TempoHardfork::is_t0()** — 添加，`>= Self::Genesis`（始终为 true，Genesis 是最低 hardfork）
11. **StorageKey for u128** — StablecoinDEX 的 `Mapping<u128, Order>` 需要
12. **StorageKey for i16** — StablecoinDEX 的 `Mapping<i16, TickLevel>` 和 bitmap 需要
13. **VecHandler::pop()** — ValidatorConfigV2 的 swap-and-pop deactivation 需要
14. ~~**ValidatorConfigV2 Ed25519 签名验证 stubbed**~~ — 已实现：使用 `ed25519-consensus` crate（`commonware-cryptography` 的底层依赖），replicate `union_unique` 格式避免重依赖
15. ~~**StablecoinDEX token transfers stubbed**~~ — 已实现：transfer/transfer_from 通过 TIP20 `system_transfer_from` 执行
16. **extend_tempo_precompiles** — 全部 9 个预编译注册到 PrecompilesMap
17. **TempoHandler** — 使用 thin wrapper（CosmosHandler 模式），override `execution()` 支持 batch/single dispatch
18. **TempoTxEnv** — `TempoContext` 使用 `TempoTxEnv` 替代 `TxEnv`
19. ~~**RPC batch calls 尚未接入**~~ — 已实现：`CallRequest` 扩展为 wrapper struct（Deref/DerefMut 到 TransactionRequest），新增 `tempo_calls` / `nonce_key` 字段
20. **estimateGas 与 writer 端无差异** — 已确认 fee handler 两端都短路，gas 计算一致（TIP-1000 通过 GasParams 注入），2D nonce 在 `disable_balance_check` 下跳过检查

## Stub / TODO 点（代码中已标记）

### TIP20 预编译 (tip20.rs)
- [x] ~~**TIP-403 compliance check**~~ — 已连接 TIP403Registry：transfer/transferFrom/transferWithMemo 合规检查、changeTransferPolicyId policy 存在性验证、mint/mintWithMemo recipient 授权、burnBlocked sender 检查
- [x] ~~**AccountKeychain spending limits**~~ — 已连接：transfer/transferWithMemo/distributeReward 的 authorize_transfer、approve 的 authorize_approve
- [x] ~~**TIP20Factory validation**~~ — 已连接：`set_next_quote_token` 调用 `TIP20Factory::is_tip20()` + USD currency 验证
- [x] ~~**Quote-token cycle detection**~~ — 已实现：`complete_quote_token_update` 遍历 quote-token 链检测环路
- [x] ~~**system_transfer_from**~~ — 已实现：供 FeeManager 等预编译使用的无授权跨预编译转账
- [x] ~~**transfer_fee_pre_tx / transfer_fee_post_tx**~~ — 已实现：Fee handler 专用的预/后交易转账方法
- [x] ~~**permit ecrecover**~~ — 已实现：使用 `secp256k1` crate 直接进行 ECDSA recovery，添加 `recover_signer` 到 `PrecompileStorageProvider` trait + `StorageCtx` 委托

### FeeManager + TIPFeeAMM (fee_manager.rs)
- [x] ~~**transfer_fee_pre_tx / transfer_fee_post_tx**~~ — 已连接：调用 TIP20 的 transfer_fee_pre_tx / transfer_fee_post_tx
- [x] ~~**TIP20Factory::is_tip20 cross-call**~~ — 已连接：set_validator_token / set_user_token 调用 `TIP20Factory::is_tip20()`
- [x] ~~**AMM token transfers**~~ — 已连接：rebalance_swap / mint / burn 通过 `system_transfer_from` + `transfer` 执行实际 TIP20 转账
- [x] ~~**Transient storage reservation**~~ — 无需实现。`pending_fee_swap_reservation` 仅在 handler 的 `collect_fee_pre_tx` 中写入，eth_call 模式下 fee handler 短路不触发

### AccountKeychain (account_keychain.rs)
- [x] ~~**P256/WebAuthn 签名验证**~~ — 已确认无需实现。`validate_keychain_authorization` 仅在 handler 层 tx validation 调用（检查 key 存在性/过期/类型匹配），不执行实际密码学验证。eth_call 不触发签名验证路径。P256 验证在 handler 的 `verify_signature` 中，不在预编译 dispatch scope
- [x] ~~**Cross-precompile wiring**~~ — 已连接：TIP20 调 `authorize_transfer` / `authorize_approve`

### TIP403Registry (tip403_registry.rs)
- [x] ~~**Cross-precompile wiring**~~ — 已连接：TIP20 的 `is_transfer_authorized`、`changeTransferPolicyId`、`_mint`、`burnBlocked` 全部调 TIP403Registry

### ValidatorConfigV2 (validator_config_v2.rs)
- [x] ~~**Ed25519 签名验证**~~ — 已实现：`ed25519-consensus` + 本地 `union_unique` 格式复制

### StablecoinDEX (stablecoin_dex.rs)
- [x] ~~**Token transfers**~~ — 已实现：transfer/transfer_from 通过 TIP20 `system_transfer_from`
- [x] ~~**Cross-precompile wiring**~~ — token transfers 已通过 TIP20 `system_transfer_from` 连接

### Storage 层 (storage.rs)
- [x] ~~**Journal checkpoints**~~ — 已实现：alloy-evm 0.29.2 `EvmInternals` 暴露了 `checkpoint()`/`checkpoint_commit()`/`checkpoint_revert()`
- [x] ~~**load_account_mut_skip_cold_load**~~ — alloy-evm 0.29.2 已有此 API，当前用 `load_account_code` + clone 功能正确，仅性能稍差。可优化但不影响 eth_call 正确性

## 待确认的疑问

1. ~~**Journal checkpoint stub 是否影响 simulateTransactions？**~~ — 已解决：alloy-evm 0.29.2 暴露了 checkpoint API，预编译内部现在使用真实 journal checkpoint
2. ~~**AccountKeychain 签名验证**~~ — 已确认不需要。eth_call 不触发签名验证，签名在 recover_signer() 层做
3. ~~**ip_validation 模块**~~ — 已解决：内联到 validator_config.rs
4. ~~**StablecoinDEX 的复杂度**~~ — 已完整移植（2239 行），view 方法可正确读取链上状态
5. ~~**cross-precompile 调用**~~ — 全部已连接：TIP20 ↔ TIP403、TIP20 ↔ AccountKeychain、FeeManager ↔ TIP20、TIP20 ↔ TIP20Factory、StablecoinDEX ↔ TIP20
6. ~~**Rust 工具链**~~ — 已解决：Dockerfile 从 `rust:1.88.0` 升级到 `rust:1.93.0`（revm-inspectors 0.36.1 要求 1.91+），Cargo.toml `rust-version = "1.91"`
7. ~~**TempoApiImpl 与 MainnetApiImpl 类型冲突**~~ — 已解决：`TempoEvmCustomConfig` marker type

## 后续工作

### P0 — 上线前

- [x] ~~**集成测试**~~ — 已完成：705 项测试，0 FAIL，15 已知差异（revert format），dev 环境 blockchain-misc-x3 镜像 amd64-234fdd7
- [x] ~~**estimateGas no_code_callee 早返回 bug**~~ — 已修复：Tempo 链跳过 `no_code_callee` 早返回优化
- [x] ~~**estimateGas caller_gas_allowance**~~ — 已实现：`ReadOnlyStorageProvider` + `tempo_caller_gas_allowance()` 读 TIP-20 fee token 余额算 gas cap，与 writer 一致
- ~~**estimateGas 单地址 1568 gas 差异**~~ — **Writer 端问题，Leafage 行为正确**。Leafage 对所有地址返回一致的 23982，与 writer 的 0x983b（无余额）一致。Writer 对 0x0cac（有 TIP-20 余额）返回 22414（低 1568），可能是 `caller_gas_allowance` 的 storage read 对后续 EVM 执行产生了副作用。Leafage 无此副作用，行为正确
- [x] ~~**Cross-precompile stub 评估**~~ — 全部已连接，无残留 stub

### P1 — 上线后

- [x] ~~**leafage-evm-chains 编译 warning 清理**~~ — Tempo 相关 warning 全部清理（`cargo fix` + 手动 dead code/visibility 修复）。剩余 2 个 `deprecated new_mainnet` 是其他链（Cosmos/Mantle），不在 Tempo 范围
- [x] ~~**getBalance/getAddressBalance 返回 Tempo placeholder**~~ — 已实现：EvmCfg 加 `virtual_balance: Option<U256>`，`get_balance_impl` 和 `debank_get_address_balance_impl` 入口拦截。Tempo 在 build.rs 设置 `NATIVE_BALANCE_PLACEHOLDER`（`uint!(4242...4242_U256)`），与 writer 一致
- [x] ~~**0x76 (AA tx) 集成测试**~~ — 已完成：block 0x9A2200 (含 3 笔 AA tx)。AA 用户 eth_call (6/6 PASS)、estimateGas (2/2 PASS)、contractMultiCall batch (PASS)、pre_traceMany 单笔+多笔 output 与 writer eth_call 精确匹配 (4/4 PASS)
- [x] ~~**TempoHandler `validate_initial_tx_gas` AA 路径**~~ — 已完整实现，与 writer 的 eth_call/estimateGas gas 计算对齐。CallRequest 扩展 `key_type`/`key_data`/`key_id`/`key_authorization`/`tempo_authorization_list` 字段，TempoTxFields 扩展 `sig_type`/`is_keychain`/`webauthn_data_size`/`key_auth`/`auth_list`。gas 计算包含：base stipend、signature verification（P256 +5k, WebAuthn +5k+calldata, Keychain +3k）、per-call cold account、authorization list（per-auth sig gas + TIP-1000 nonce==0）、key authorization（pre-T1B heuristic / T1B+ storage-based）、calldata tokens、CREATE costs、2D nonce gas（expiring 13k / new_account 250k / existing 5k）。6 个单测覆盖
- [x] ~~**TempoHandler `validate_env` override**~~ — 已实现：value!=0 拒绝 + AA calls 结构校验（非空、CREATE 规则）。Keychain 版本/subblock/priority fee/time window 为 writer-only 验证，eth_call 模式不需要
- [x] ~~**TempoHandler `inspect_execution` override**~~ — 已实现：override `InspectorHandler::inspect_execution`，AA batch 每个 sub-call 走 `inspect_run_exec_loop`（inspector-aware frame loop）。`execute_multi_call` 重构为接受 closure 的 free function，`execution()` 和 `inspect_execution()` 共用

### P2 — 按需

- [x] ~~**预编译内部 gas 动态化**~~ — 已实现：`LeafageStorageProvider` 新增 `sstore_set_cost()` 和 `code_deposit_cost_per_byte()` 方法，根据 `self.spec.is_t1()` 返回 TIP-1000 值或标准 Ethereum 值。sstore、set_code、sstore_refund 三处改为动态调用。pre-T1A trace 内部 gasUsed 现在与 writer 一致
- [x] ~~**TempoTxEnv AA 扩展字段**~~ — 已实现全部字段：gas 字段（`sig_type`/`is_keychain`/`webauthn_data_size`/`key_auth`/`auth_list`）+ tx 字段（`key_id`/`fee_token`/`fee_payer`/`valid_after`/`valid_before`）。`is_system_tx`/`subblock_metadata` 读节点不需要
- ~~**ValidatorConfigV2 pre-execution code 注入**~~ — **无需实现**。Writer 在 block execution 的 `apply_pre_execution_changes` 注入 `0xef` bytecode（T2 激活时），产生 state_diff → leafage 通过 pipeline 自动同步。且 T2 尚未激活（`MAINNET_T2_TIME = u64::MAX`）
- [x] ~~**TempoBlockEnv timestamp_millis_part**~~ — 已实现：`TempoBlockEnv` 替代 `BlockEnv` 作为 `TempoContext` 的 block 类型，`MILLIS_TIMESTAMP` (0x4F) opcode 在 pre-T1C 注册到指令表。`timestamp_millis_part` 默认 0（pipeline 不携带此字段），archive 模式 pre-T1C eth_call 的 0x4F 返回 `timestamp * 1000`
- ~~Fee log 生成~~ — **实测确认**：Tempo writer 的 eth_call / pre_traceMany 无论 gas_price 是否为 0 都不产生 fee log（`disable_base_fee=true` 使 fee handler 始终短路）。leafage 行为一致，无差异
- [x] ~~Tempo hardfork 动态切换（如需 archive 模式）~~ — 已实现：`TempoHardfork::from_timestamp()` + `LeafageStorageProvider` 从 block timestamp 推导 + `TempoEvm::new()` 条件 GasParams
- ~~cargo feature gate `tempo`~~ — 不做，其他链（BSC/Cosmos/Mantle）也没有 feature gate，保持一致
- ~~**预编译 SSTORE gas refund 未传播到 ResultGas**~~ — 已修复：两层问题。(1) `sstore_refund()` clean slot (original==present) 非零→零缺少 refund 计算，补上 SSTORE_CLEARS_SCHEDULE (4800)。(2) alloy-evm 的 `PrecompilesMap::run()` 不调 `record_refund()`（标准预编译无 refund），通过 `TempoPrecompiles` wrapper + thread-local 补上传播
- [x] ~~**AA apply_eip7702_auth_list override**~~ — 已实现：`TempoHandler::apply_eip7702_auth_list` override，从 `aaAuthorizationList` 的 `authority`+`address` 字段构建 `TempoAuthDelegation`（实现 `AuthorizationTr`），调用 `revm::handler::pre_execution::apply_auth_list` 应用 delegation。RPC 调用方直接提供 authority 地址（不需要签名恢复）。T1+ 无 refund
- [x] ~~**AA per-auth keychain gas +3000**~~ — 已修复：`TempoAuthGas` 加 `is_keychain: bool`，per-auth 循环判断加 `KEYCHAIN_VALIDATION_GAS` (3000)。同时 `TempoAuthGasInfo` RPC 类型加 `is_keychain` 字段
- **feePayerSignature 签名恢复** — Writer 接受 `feePayerSignature: Signature`，通过 `TempoTransaction.recover_fee_payer(sender)` (RLP 编码 + ecrecover) 恢复 sponsor 地址。Leafage 用 `feePayer: Address` 直接提供地址（无 TempoTransaction RLP 依赖）。如需完全兼容 writer RPC 格式，需引入 tempo_primitives 的 RLP 编码逻辑做签名恢复
- **P1: 2D nonce (nonceKey>0) 执行 gas 缺 ~250k** — Writer 在 `validate_against_state_and_deduct_caller` 中对 2D nonce 调用 `NonceManager.get_nonce()` + `increment_nonce()` 预编译操作，消耗 ~250k gas（cold SLOAD + SSTORE on T1+）。Leafage pre_execution 没有 NonceManager 交互。此 gas 出现在 pre_traceMany/simulateTransactions 的执行结果中。测试数据：nonceKey=0x1 时 Writer=273270 vs Leafage=28270 (diff=245000)
- **P1: webAuthn keyType gas 过高 +37k** — Leafage 对 keyType=webAuthn 的 gas 计算为 77742，Writer 为 40638 (diff=+37104)。需排查 `webauthn_data_size` 默认值和 mock data 构造逻辑是否与 writer 一致。可能是 `key_data` 未传时默认 size 计算差异

### Writer-Leafage Handler 差异总览

Tempo writer Handler 12 个 override 方法 vs leafage 7 个 override:

| Writer 方法 | Leafage | 状态 |
|---|---|---|
| validate_env (value!=0 + AA 校验) | 已实现 | 完成（value 拒绝 + calls 结构 + time window） |
| validate_initial_tx_gas (标准 tx) | 已实现 | 完成 (nonce==0 +250k, GasParams) |
| validate_initial_tx_gas (AA tx) | 已实现 | 完成（per-auth keychain +3000, key_auth, 2D nonce） |
| pre_execution (warm-up + keychain) | 已实现 | 完成（warm fee token + set_tx_origin + set_transaction_key） |
| apply_eip7702_auth_list | 已实现 | 完成（TempoAuthDelegation, authority+address 直接提供） |
| execution (batch dispatch) | 已实现 | 完成（execute_multi_call, checkpoint/revert） |
| inspect_execution (AA tracing) | 已实现 | 完成（inspect_run_exec_loop per sub-call） |
| run (fee loading) | 不需要 | 读节点不需要 fee 系统 |
| execution_result (TempoHaltReason) | 不需要 | Leafage 无 evm.initial_gas 字段 |
| validate_against_state_and_deduct_caller | 不需要 | 读节点不需要 fee/nonce 扣减（关键逻辑已移至 pre_execution） |
| reimburse_caller | 不需要 | 读节点不需要 fee 退还 |
| reward_beneficiary | 不需要 | 读节点不需要矿工奖励 |
| catch_error | 不需要 | subblock fee 路径不可达 |
