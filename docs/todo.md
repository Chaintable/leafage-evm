- `[2026-08-11][done] Arbitrum EIP-7825 豁免保留显式 RPC gas cap` — 当前 `cfg_for_tx` 无条件设置 `u64::MAX`，会覆盖 standalone 配置的 RPC cap。
  **Decision:** 仅在 `tx_gas_limit_cap` 为 `None` 或 `Some(0)` 时设置 `u64::MAX`；显式非零 cap 原样保留，并用 Arbitrum API 构造路径及 16.7M/100M 边界测试验证。
  **Done:** `arbitrum_osaka_does_not_reject_mainnet_eip7825_plus_one` 与 `arbitrum_osaka_preserves_explicit_rpc_gas_cap` 均通过。
- `[2026-08-11][done] Arbitrum API 测试使用最小 BlockIndex` — 原测试辅助对象使用 `DB=()`，无法调用 `EvmExecutor::create_txn_env`，导致旧测试只能绕过 Arbitrum API 调公共 helper。
  **Decision:** 在测试模块增加不返回区块的 `TestBlockIndex`，仅用于满足真实 API 路径的 trait bound。
  **Done:** `rpc_gas_cap_clamps_request_before_arbitrum_execution` 已改为调用 `ArbitrumApiImpl::create_txn_env` 并通过。
- `[2026-08-11][done] 共享 estimate 逻辑中 Some(0) 的语义` — `debank.rs` 的 gas 上限计算对所有链生效，需要明确此次跨链修复。
  **Decision:** `Some(0)` 表示不设置 RPC cap，但仍受链的 consensus cap 和 block gas limit 限制；增加非 Arbitrum 边界单测。
  **Done:** `zero_rpc_gas_cap_is_unlimited_but_consensus_and_block_caps_still_apply` 通过；`leafage-evm-rpc` 全部 61 个单测及 2 个集成测试通过。
