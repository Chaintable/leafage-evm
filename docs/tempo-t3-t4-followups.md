# Tempo T3 / T4 Adaptation — Follow-up TODOs

## Context

The branch `feature/tempo-t3-t4-adaptation` lands the bulk of the Tempo T3
(activated 2026-04-27 14:00 UTC) and T4 (activates 2026-05-18 14:00 UTC)
adaptation for the generic-node side. It covers hardfork routing, two new
T3+ precompiles (signature_verifier, address_registry), TIP-20 paused / virtual
forwarding / rewards rejection, stablecoin_dex T4 paused gates, and the
key-auth gas formula through T4.

Below are the items intentionally **not** in that PR. Each has a short rationale,
an estimated size, dependencies, and the writer code reference for mirroring.

Original brief: `~/code/task_tempo/leafage-evm-t3-t4-handover.md`
Original plan: `~/.claude/plans/mellow-tickling-fairy.md`
Tempo writer (reference): `~/code/task_tempo/`, branch `merge/upstream-v1.7.0`

---

## Status overview

| ✅ in PR | ⏸ remaining |
|---|---|
| hardfork.rs T3+T4 timestamp routing + `is_t4()` + gas table | consensus_context field in eth_getBlockByNumber (FU-7) |
| `tempo/address.rs` virtual address helpers (TIP-1022) | dev-node eth_call byte-equivalent regression (FU-10) |
| `PrimitiveSignature::from_bytes` / `recover_signer` | TIP-1016 state gas when mainnet flag flips (FU-11) |
| TIP-1020 signature_verifier precompile | |
| TIP-1022 address_registry precompile | |
| TIP-20 mint/burn paused gate (TIP-1038 #2, T3+) | |
| TIP-20 virtual recipient forwarding (T3+) | |
| stablecoin_dex T4 paused token check | |
| key_auth_gas T3/T4 + scope-driven helpers | |
| TIP-20 rewards `set_reward_recipient` virtual rejection | |
| Account-keychain CallScope wire-up incl. setAllowedCalls / getAllowedCalls / removeAllowedCalls (FU-1) | |
| `FixedBytes<4>` storage primitives + `ContractStorageReader` (FU-1a, FU-1b) | |
| KeyAuthorization carries `allowedCalls` → `ScopeCounts` (FU-2) | |
| `SpendingLimitState` 4-field 2-slot layout (FU-3) | |
| `refund_spending_limit` T3+ clamp to `max` (FU-4) | |
| `validate_call_scopes` T3 stateful / T4 stateless target check (FU-5) | |
| T3+ periodic spending-limit reset (FU-6) | |
| Rewards-path virtual-rejection audit — no new sites (FU-8) | |
| TIP-1038 #1 `setUserToken` T3+ read-before-write (FU-9) | |

---

## FU-1 — Wire account_keychain CallScope storage read/write

**Status**: ✅ Resolved in commit `7706744`. ABI renamed to writer-mirror
(`setAllowedCalls` / `getAllowedCalls` / `removeAllowedCalls`); dispatch
replaced with full impls of three-layer KeyScope/TargetScope/SelectorScope
storage read/write using slot-computation helpers. Added `SetHandler::insert`
and `remove` (OZ EnumerableSet single-element ops).

### FU-1a — Add `FixedBytes<4>` storage-primitive traits

**Status**: ✅ Resolved in commit `620ed58`. Mirrors writer macro pattern
(value in lower N bytes, default left-pad mapping key).

### FU-1b — Per-contract `StorageOps` adapter around `StorageCtx`

**Status**: ✅ Resolved in commit `c51b436`. Added `ContractStorageReader`
(read-only adapter; writes go through typed `Slot`/`Mapping` handlers).

---

## FU-2 — Carry `allowedCalls` through `KeyAuthorization` parsing

**Status**: ✅ Resolved in commit `1d56765`. Added wire-level
`CallScope`/`SelectorRule` types in `leafage-evm-types::rpc::call`,
extended `TempoKeyAuthGasInfo.allowed_calls`, reworked manual RLP encoder
for the 3-position trailing-canonical accounting, and added
`derive_scope_counts` in `leafage-evm-rpc` so the AA gas formula gets
byte-accurate `ScopeCounts` end-to-end.

---

## FU-3 — `SpendingLimitState` 4-field 2-slot storage layout

**Status**: ✅ Resolved in commit `59a2e44`. Added `SpendingLimitState`
struct + manual 2-slot `Storable` impl + field-level
`SpendingLimitStateHandler` mirroring writer's auto-derive. Migrated
`spending_limits` mapping type and all 5 callers; pre-T3 wire (slot+1 == 0)
decodes to non-periodic semantics.

---

## FU-4 — `refund_spending_limit` T3+ clamp to original max

**Status**: ✅ Resolved in commit `0ac5f8e`. T3+ clamps
`remaining + amount` to `state.max`; pre-T3 keeps the saturating-add.

---

## FU-5 — `validate_call_scopes` T3 stateful target check

**Status**: ✅ Resolved in commit `8a93c60`. T3 uses
`TIP20Factory::is_tip20` storage probe; T4+ uses stateless
`TempoAddressExt::is_tip20` prefix check.

---

## FU-6 — T3 periodic spending-limit reset logic

**Status**: ✅ Resolved in commit `b850804`. `verify_and_update_spending`
on T3+ rolls over the window when current timestamp ≥ period_end:
advances period_end by full period(s) (multi-period skip supported) and
refills `remaining = max` before applying the spend. Pure helper
`SpendingLimitState::compute_next_period_end` mirrors writer L151-160.

---

## FU-7 — Expose `consensus_context` in `eth_getBlockByNumber`

**Why deferred**: post-T4 blocks carry a `consensus_context: Option<TempoConsensusContext>`
field (`{epoch, view, parent_view, proposer}`). leafage's `block.rs` uses
`revm::context::BlockEnv` (not `alloy::rpc::types::Header`), so this field is
silently dropped during parsing. Business consumers (background-tracer,
DeBankCore) currently don't use it, so it was de-scoped.

**Writer reference**: `crates/primitives/src/header.rs` (`TempoHeader`
struct + `TempoConsensusContext`).

**Files**:
- `crates/leafage-evm-chains/src/tempo/block.rs` (extend env type with optional field)
- RPC serialization layer where `eth_getBlockByNumber` builds the response

**Decision required**: confirm with business team whether they need this field
before doing the work. If yes:

**Estimate**: ~80 lines.

---

## FU-8 — Other TIP-20 rewards-path virtual rejections

**Status**: ✅ Audited (0 code changes). Writer
`crates/precompiles/src/tip20/rewards.rs` `is_virtual()` greps revealed
exactly one production rejection site at L139 (`set_reward_recipient`);
the L752+ occurrence is test code. Leafage already implements the
equivalent rejection at `tempo/precompile/tip20.rs:1599`. No additional
recipient-bearing entry points exist in the rewards path.

---

## FU-9 — TIP-1038 #1 `setUserToken` read-before-write optimisation

**Status**: ✅ Resolved in commit `a4b5c82`. T3+ `set_user_token` reads
the current preferred token after USD validation and returns `Ok(())`
early when it equals the requested value, skipping both the SSTORE and
the `UserTokenSet` event emission. Pre-T3 behaviour unchanged.

---

## FU-10 — Dev-node eth_call byte-equivalent regression

**Why deferred**: needs a running leafage instance and the dev Tempo
writer container, plus picking and pinning mainnet block heights for the
fixtures. Out of scope for the code-only PR.

**Dev environment**:
- `blockchain-misc-x3` host
- `tempo-dev` container, HTTP port 8566
- Writer image: `294354037686.dkr.ecr.ap-northeast-1.amazonaws.com/blockchain/tempo:d6e55f6`

**Test matrix** (run after the PR merges; pick 5-10 post-T3 mainnet blocks
plus 5-10 post-T4 blocks once T4 activates 2026-05-18):

| RPC call | Both leafage & writer should return byte-equal | Covered by |
|---|---|---|
| `eth_call signature_verifier.recover(hash, secp256k1_sig)` | identical address | PR ✅ |
| `eth_call signature_verifier.verify(addr, hash, p256_sig)` | identical bool | PR ✅ |
| `eth_call signature_verifier.verify(addr, hash, webauthn_sig)` | identical bool | PR ✅ |
| `eth_call address_registry.resolveRecipient(virtual_addr)` | identical master | PR ✅ |
| `eth_call tip20.balanceOf(virtual_addr)` | identical balance | PR ✅ |
| `eth_call tip20.balanceOf(master_addr)` after sim transfer to virtual | identical balance | PR ✅ |
| `eth_call tip20.mint(...)` on paused token (T3+) | identical revert reason | PR ✅ |
| `eth_call account_keychain.getKey(account, keyId)` | identical struct | PR ✅ |
| `eth_call account_keychain.getAllowedCalls(account, keyId)` | identical `(bool, CallScope[])` | FU-1 ✅ |
| `eth_estimateGas` AA tx with only spending limits | byte-identical | PR ✅ |
| `eth_estimateGas` AA tx with call scopes | byte-identical (was off until FU-2) | FU-2 ✅ |
| `eth_call account_keychain.getRemainingLimit` post-T3 periodic | byte-identical (was off until FU-3) | FU-3 ✅ |
| `eth_call stablecoin_dex` order placement on paused token (T4+) | identical revert | PR ✅, validate post-5/18 |
| `eth_call tip20.set_reward_recipient(virtual)` (T3+) | identical InvalidRecipient revert | PR ✅ |

**Acceptance**: every covered row returns byte-equal bytes to writer; any
divergence becomes a follow-up bug.

---

## FU-11 — TIP-1016 state gas (when the writer flag flips)

**Why deferred**: `cfg_env.enable_amsterdam_eip8037` is the writer gate.
It is **disabled on mainnet** at T4 activation; writer state_gas is 0,
leafage's `key_auth_gas` returns plain `u64` which is correct. If the flag
ever flips on mainnet (presumably a future hardfork), we'll need to track
state_gas alongside total_gas.

**Writer reference**: `crates/revm/src/handler.rs:302-378`
(`calculate_key_authorization_gas` returns `(total_gas, state_gas)` tuple).

**Steps**:
1. Change `key_auth_gas` return type from `u64` to `(u64, u64)`.
2. Inside the T4+ branch, when the new flag is set (will need a new
   `TempoHardfork` variant or `cfg_env` field once writer defines one),
   compute `state_gas = sstore_set_state_gas * num_sstores`.
3. Propagate `state_gas` to `calculate_aa_batch_intrinsic_gas` so callers
   can see both numbers (RPC `eth_estimateGas` returns total gas; state
   gas matters for the chain's separate accounting).

**Estimate**: ~100 lines. Re-trigger only when the writer flag flips
on-chain.

---

## Remaining items

Only three items are still outstanding:

1. **FU-10** (dev-node regression) — run continuously, not a one-shot;
   covered by `docs/test-plan-tempo-t3-t4.md` §1. Activate once the
   leafage dev replica (`blockchain-misc-x1`) catches up to post-T3 tip.
2. **FU-7** (consensus_context) — only do if business confirms a need.
3. **FU-11** (TIP-1016 state gas) — only do if the writer
   `enable_amsterdam_eip8037` flag flips on mainnet.
