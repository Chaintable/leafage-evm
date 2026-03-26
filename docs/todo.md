# Tempo Adaptation TODO / 决策记录

## 已做的决策

1. ~~**不实现 AA tx (0x76) 完整执行路径**~~ — 已实现批量原子执行（TempoTxEnv + TempoHandler::execute_multi_call），不需要 fee handler 和签名验证
2. **Fee handler 无需实现** — 已确认 Tempo writer 的 `eth_call` / `eth_estimateGas` 在 `disable_balance_check=true` 下，handler 的 `validate_against_state_and_deduct_caller` 中 `gas_balance_spending=0` 自动短路跳过 fee 收取。leafage 行为一致
3. **Hardfork 策略** — 最小 TempoHardfork 枚举，所有 is_*() 返回 true，只跑最新 spec
4. **revm 版本** — 已升级到 revm 36.0.0 (op-revm 17.0.0, revm-inspectors 0.36.1, revm-bytecode 9.0.0, alloy-evm 0.29.2)
5. **GasParams** — 已使用 revm 36 原生 GasParams API 设置 Tempo TIP-1000 gas 参数覆盖（TempoEvm::new 中 7 项 override），同时保留 TempoGasCosts 常量供 LeafageStorageProvider 使用
6. **编译工具链** — Rust 1.93.0（revm 36 要求 1.88+, revm-inspectors 0.36.1 要求 1.91+）
7. **StorageKey for u64** — 为 `u64` 添加 `StorageKey` impl（TIP403Registry 的 `Mapping<u64, PolicyRecord>` 需要）
8. **AccountKeychain 签名验证不需要 p256/sha2 crate** — leafage 只读取 keychain state，不做签名验证。实际密码学验证在 tx handler 层（不在预编译 scope）
9. **TempoPrecompileError::under_overflow()** — 添加 Panic(0x11) 辅助方法
10. **TempoHardfork::is_t0()** — 添加，与其他 is_*() 一样始终返回 true
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
- [ ] **permit ecrecover** — 返回 InvalidSignature（genuine limitation：leafage 无 ecrecover 访问，permit 是写操作不影响 eth_call）

### FeeManager + TIPFeeAMM (fee_manager.rs)
- [x] ~~**transfer_fee_pre_tx / transfer_fee_post_tx**~~ — 已连接：调用 TIP20 的 transfer_fee_pre_tx / transfer_fee_post_tx
- [x] ~~**TIP20Factory::is_tip20 cross-call**~~ — 已连接：set_validator_token / set_user_token 调用 `TIP20Factory::is_tip20()`
- [x] ~~**AMM token transfers**~~ — 已连接：rebalance_swap / mint / burn 通过 `system_transfer_from` + `transfer` 执行实际 TIP20 转账
- [ ] **Transient storage reservation** — omitted（leafage 是只读节点）

### AccountKeychain (account_keychain.rs)
- [x] ~~**P256/WebAuthn 签名验证**~~ — 已确认无需实现。eth_call 不触发签名验证路径
- [x] ~~**Cross-precompile wiring**~~ — 已连接：TIP20 调 `authorize_transfer` / `authorize_approve`

### TIP403Registry (tip403_registry.rs)
- [x] ~~**Cross-precompile wiring**~~ — 已连接：TIP20 的 `is_transfer_authorized`、`changeTransferPolicyId`、`_mint`、`burnBlocked` 全部调 TIP403Registry

### ValidatorConfigV2 (validator_config_v2.rs)
- [x] ~~**Ed25519 签名验证**~~ — 已实现：`ed25519-consensus` + 本地 `union_unique` 格式复制

### StablecoinDEX (stablecoin_dex.rs)
- [x] ~~**Token transfers**~~ — 已实现：transfer/transfer_from 通过 TIP20 `system_transfer_from`
- [ ] **Cross-precompile wiring** — TIP20Factory.is_tip20, TIP403Registry.is_authorized_as 等

### Storage 层 (storage.rs)
- [x] ~~**Journal checkpoints**~~ — 已实现：alloy-evm 0.29.2 `EvmInternals` 暴露了 `checkpoint()`/`checkpoint_commit()`/`checkpoint_revert()`
- [ ] **load_account_mut_skip_cold_load** — 用 load_account_code + clone 替代

## 待确认的疑问

1. ~~**Journal checkpoint stub 是否影响 simulateTransactions？**~~ — 已解决：alloy-evm 0.29.2 暴露了 checkpoint API，预编译内部现在使用真实 journal checkpoint
2. ~~**AccountKeychain 签名验证**~~ — 已确认不需要。eth_call 不触发签名验证，签名在 recover_signer() 层做
3. ~~**ip_validation 模块**~~ — 已解决：内联到 validator_config.rs
4. ~~**StablecoinDEX 的复杂度**~~ — 已完整移植（2239 行），view 方法可正确读取链上状态
5. ~~**cross-precompile 调用**~~ — TIP20 ↔ TIP403、TIP20 ↔ AccountKeychain、FeeManager ↔ TIP20、TIP20 ↔ TIP20Factory 全部已连接。仅 StablecoinDEX token transfer 保留 stub（view 方法不受影响）
6. **Rust 工具链** — 项目没有 rust-toolchain.toml。当前用 1.93.0，CI 需确认
7. ~~**TempoApiImpl 与 MainnetApiImpl 类型冲突**~~ — 已解决：`TempoEvmCustomConfig` marker type

## 后续工作

### P0 — 上线前

- [ ] **集成测试** — 对照 dev 环境（blockchain-misc-x3, 端口 8566）验证
- [ ] **Cross-precompile stub 评估** — 集成测试中确认是否影响 DeBankCore

### P2 — 按需

- [ ] Fee log 生成（如 DeBankCore 需要 fee Transfer log）
- [ ] Tempo hardfork 动态切换（如需 archive 模式）
- [ ] cargo feature gate `tempo`
