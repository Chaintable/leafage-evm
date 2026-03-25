# Tempo Adaptation TODO / 决策记录

## 已做的决策

1. **不实现 AA tx (0x76) 完整执行路径** — 工作量大（BSC 3-5x），先简单版本上线
2. **跳过 Fee handler / Fee log** — 与 writer 端 pre_traceMany 行为一致（disable_base_fee）
3. **Hardfork 策略** — 最小 TempoHardfork 枚举，所有 is_*() 返回 true，只跑最新 spec
4. **revm 版本** — 不升级 revm（保持 33.1），用 LeafageStorageProvider 适配层桥接
5. **GasParams** — 不使用 revm 36 的 GasParams API，改为 TempoGasCosts hardcoded 常量
6. **编译工具链** — 使用 Rust 1.87.0（1.93.1 与 tokio 不兼容）

## Stub / TODO 点（代码中已标记）

### TIP20 预编译 (tip20.rs)
- [ ] **TIP-403 compliance check** — `is_transfer_authorized` 被 stub 为始终返回 `Ok(true)`，等 TIP403Registry 移植后（Task 4b）连接
- [ ] **AccountKeychain spending limits** — 被 stub，等 AccountKeychain 移植后连接
- [ ] **TIP20Factory validation** — `set_next_quote_token` 中的工厂验证被推迟到 Task 4a
- [ ] **permit ecrecover** — 返回 InvalidSignature，因为 storage provider 未暴露 ecrecover

### NonceManager (nonce.rs)
- [x] **Core logic ported** -- `get_nonce`, `increment_nonce`, `is_expiring_nonce_seen`, `check_and_mark_expiring_nonce`
- [x] **Dispatch** -- only `getNonce` view exposed via ABI dispatch (write methods are internal-only, called by tx execution)

### FeeManager + TIPFeeAMM (fee_manager.rs)
- [ ] **transfer_fee_pre_tx / transfer_fee_post_tx** -- stubbed, these are fee-handler-specific TIP20 methods not present in the ported TIP20. Actual token transfers during fee collection are skipped; pool reserve accounting works correctly
- [ ] **TIP20Factory::is_tip20 cross-call** -- stubbed to `is_tip20_prefix` check (validates address prefix only, not bytecode existence). Full cross-call needs TIP20Factory wired
- [ ] **AMM token transfers in rebalance_swap/mint/burn** -- pool reserve math is ported, but actual TIP20 `system_transfer_from` / `transfer` calls for token movement are stubbed
- [ ] **Transient storage reservation** -- `pending_fee_swap_reservation` T1C+ logic omitted (leafage is read-only, transient storage is per-tx)

### TIP20Factory (tip20_factory.rs)
- [x] **Core logic ported** -- `create_token`, `create_token_reserved_address`, `is_tip20`, `get_token_address`, `compute_tip20_address`
- [x] **Dispatch** -- `createToken` (mutate), `isTIP20` (view), `getTokenAddress` (view)

### ValidatorConfig (validator_config.rs)
- [x] **Core logic ported** -- owner management, validator CRUD, DKG ceremony epoch, status changes
- [x] **ip_validation inlined** -- `ensure_address_is_ip_port` inlined as local function (was question #3 -- resolved: used by ValidatorConfig for address validation)
- [x] **Hardfork gating** -- `changeValidatorStatusByIndex` always available (leafage runs latest spec, no T0/T1 distinction)
- [x] **Validator Storable** -- manually implemented packed storage layout matching `#[derive(Storable)]` output

### Storage 层 (storage.rs)
- [ ] **Journal checkpoints** — 被 stub（EvmInternals 0.25.2 不暴露 checkpoint 操作）。leafage 是只读场景，实际不需要 checkpoint，但如果 simulateTransactions 中的 mutate 操作需要 rollback 语义，可能有问题
- [ ] **load_account_mut_skip_cold_load** — 用 load_account_code 替代，account info 被 clone 以避免借用冲突。性能影响待验证

## 待确认的疑问

1. **Journal checkpoint stub 是否影响 simulateTransactions？** — simulateTransactions 是顺序执行 + commit。如果某个预编译内部用 checkpoint + revert 做原子操作（如 TIP20 transfer 失败回滚），stub 后行为可能不正确
2. **AccountKeychain 签名验证** — P256/WebAuthn 签名验证是否需要完整实现？还是 leafage eth_call 不触发？建议先 stub，集成测试时确认
3. ~~**ip_validation 模块**~~ — 已解决：ValidatorConfig 使用它做 inbound/outbound 地址验证，已内联到 validator_config.rs
4. **StablecoinDEX 的复杂度** — 4952 行最大预编译，CLOB 订单簿。需要评估是否完整移植还是 stub 不常用的方法
5. **cross-precompile 调用** — TIP20 transfer 调 TIP403、FeeManager 调 TIP20 等。当前各预编译独立移植 + stub，最后需要连接起来验证
6. **Rust 工具链** — 项目没有 rust-toolchain.toml，CI 用什么版本？是否需要添加？
7. **TempoApiImpl 与 MainnetApiImpl 类型冲突** — 两者都是 `ApiImpl<DB, MainnetSpecId, NoneEvmCustomConfig>`，EvmExecutor 不能对同类型 impl 两次。需要用 marker type 或独立 struct 区分

## 后续工作（当前 scope 外）

- [ ] AA tx 完整执行路径（如需求升级）
- [ ] Fee log 生成（如 DeBankCore 需要）
- [ ] Tempo hardfork 动态切换（如需支持历史区块查询）
- [ ] cargo feature gate `tempo`（减少非 Tempo 链的编译时间）
