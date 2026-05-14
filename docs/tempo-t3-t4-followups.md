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

| ✅ in PR | ⏸ in this TODO |
|---|---|
| hardfork.rs T3+T4 timestamp routing + `is_t4()` + gas table | (`call_scope_storage_slots` byte-accuracy needs ScopeCounts wired through parsing — FU-2) |
| `tempo/address.rs` virtual address helpers (TIP-1022) | (KeyAuthorization carrying `allowedCalls` — FU-2) |
| `PrimitiveSignature::from_bytes` / `recover_signer` | (SpendingLimitState 4-field 2-slot layout — FU-3) |
| TIP-1020 signature_verifier precompile | (`refund_spending_limit` T3+ clamp — FU-4) |
| TIP-1022 address_registry precompile | (validate_call_scopes T3 stateful target check — FU-5) |
| TIP-20 mint/burn paused gate (TIP-1038 #2, T3+) | (CallScope storage read/write — FU-1) |
| TIP-20 virtual recipient forwarding (T3+) | (T3 periodic spending-limit reset logic — FU-6) |
| stablecoin_dex T4 paused token check | (consensus_context field in eth_getBlockByNumber — FU-7) |
| key_auth_gas T3/T4 + scope-driven helpers | (rewards-path additional virtual rejections — FU-8) |
| TIP-20 rewards `set_reward_recipient` virtual rejection | (TIP-1038 #1 setUserToken read-before-write — FU-9) |
| Account-keychain CallScope ABI + storage layout (scaffolding) | (dev-node eth_call byte-equivalent regression — FU-10) |
| ScopeCounts threading through TempoKeyAuthGas | (TIP-1016 state gas when mainnet flag flips — FU-11) |

---

## FU-1 — Wire account_keychain CallScope storage read/write

**Why deferred**: blocked on FU-1a + FU-1b (leafage framework gaps). The current
PR lands the ABI surface and storage-layout reservation but the dispatch entries
return `InvalidCallScope` (write) and `Vec::new()` (read). On-chain state
populated by writer is *physically* on the right storage slots in leafage's
state tree (state diffs apply correctly); only the read/write logic that walks
the slots is missing.

**Files**: `crates/leafage-evm-chains/src/tempo/precompile/account_keychain.rs`
(see source comment at the `call_scope_base` field for the on-chain layout).

**Writer references**:
- `crates/precompiles/src/account_keychain/mod.rs:93-128` (Solidity-equivalent layout)
- `crates/precompiles/src/account_keychain/mod.rs:204-239` (storage wiring)
- `crates/precompiles/src/account_keychain/mod.rs:831-922` (validate_call_scopes)

**Estimate**: ~400 lines (after FU-1a/FU-1b unblock) + storage byte-by-byte
dev-node comparison.

**Acceptance**:
1. `getCallScope(account, keyId)` returns the same CallScope[] as writer for
   an arbitrary post-T3 mainnet block.
2. `setCallScopes` in an `eth_call` produces the same state diff as writer
   `eth_call` against the same RPC payload (writes don't persist in either
   case, so this is a pure read-back round-trip inside the simulation).

### FU-1a — Add `FixedBytes<4>` storage-primitive traits

**Why**: `SetHandler<FixedBytes<4>>` is needed for the per-target selector
set. leafage's `StorageKey`, `Storable`, `StorableType`, `Packable`, `FromWord`,
`sealed::OnlyPrimitives` are all sealed on specific types (`bool`, `Address`,
`U256`, `u64`, `u128`, `i16`, `B256`). `FixedBytes<4>` (bytes4) has no impls.

**File**: `crates/leafage-evm-chains/src/tempo/precompile/storage_types.rs`

**Approach**: mirror the existing `impl StorageKey for B256` and
`impl Packable for U256` style. Solidity ABI right-pads `bytes4` to 32 bytes
when used as a mapping key, so `as_storage_bytes` returns the 4 raw bytes;
`mapping_slot` already pads with zeros (verify it pads on the **right**, not
left — `bytes4` differs from `address`/`uint` here).

**Estimate**: ~80 lines for `FixedBytes<4>`. Optionally generalise to
`FixedBytes<N>` for `N <= 32`.

### FU-1b — Per-contract `StorageOps` adapter around `StorageCtx`

**Why**: `Set::load` and free-standing `Storable::load` calls take `&impl
StorageOps`. The only producers of `StorageOps` currently are leafage's
`Slot` / `Mapping` handlers (which know the contract address). For
account_keychain to load a `Set<Address>` at a manually-computed slot, we
need an adapter that ties `StorageCtx` to `ACCOUNT_KEYCHAIN_ADDRESS`.

**File**: `crates/leafage-evm-chains/src/tempo/precompile/storage.rs`

**Approach**:
```rust
pub struct ContractStorageOps<'a> {
    storage: &'a StorageCtx,
    address: Address,
}
impl<'a> StorageOps for ContractStorageOps<'a> {
    fn load(&self, slot: U256) -> Result<U256> {
        self.storage.sload(self.address, slot)
    }
    fn store(&mut self, _slot: U256, _value: U256) -> Result<()> {
        // For write paths, leafage precompiles already go through Slot/Mapping
        // handlers which take a `&mut StorageCtx`. This adapter is primarily
        // for the read path on heterogeneous nested structures.
        unreachable!("ContractStorageOps is read-only; use Slot/Mapping for writes")
    }
}
```

Or split into `ContractStorageReader` / `ContractStorageWriter` to avoid the
unreachable.

**Estimate**: ~40 lines + 1 unit test.

---

## FU-2 — Carry `allowedCalls` through `KeyAuthorization` parsing into `TempoKeyAuthGas.scope_counts`

**Why deferred**: `crates/leafage-evm-chains/src/tempo/fee_payer.rs::KeyAuthorization`
currently has fields `{chain_id, key_type, key_id, expiry, limits}` — no
`allowedCalls`. tx-envelope parsing populates `TempoKeyAuthGas.scope_counts`
from that struct, so until the field exists, scope_counts is always
`Default::default()` (`has_allowed_calls = false`, everything zero).

`key_auth_gas` already accepts `scope_counts` and produces byte-accurate gas
when fed the right values — only the data source is missing.

**Files**:
- `crates/leafage-evm-chains/src/tempo/fee_payer.rs` (extend KeyAuthorization)
- wherever `TempoKeyAuthGas` is populated from a parsed transaction (grep for
  `TempoKeyAuthGas { sig_type:` constructor calls)

**Writer reference**: `crates/contracts/src/precompiles/account_keychain.rs:59-68`
(`KeyRestrictions` adds `allowAnyCalls: bool` and `allowedCalls: CallScope[]`).

**Implementation sketch**:
1. Add `pub allow_any_calls: bool` and `pub allowed_calls: Option<Vec<CallScope>>`
   to `KeyAuthorization` (`CallScope` ABI type already in
   `IAccountKeychain::CallScope` — re-export or duplicate the in-memory struct).
2. When parsing the transaction envelope (or RPC request) into `TempoTxFields`,
   compute `ScopeCounts` from `key_authorization.authorization.allowed_calls`:
   ```rust
   let scope_counts = match &auth.allowed_calls {
       None => ScopeCounts::default(),
       Some(scopes) => ScopeCounts {
           has_allowed_calls: true,
           scopes: scopes.len() as u32,
           selectors: scopes.iter().map(|s| s.selector_rules.len()).sum::<usize>() as u32,
           constrained_selectors: scopes.iter()
               .flat_map(|s| &s.selector_rules)
               .filter(|r| !r.recipients.is_empty())
               .count() as u32,
           recipients: scopes.iter()
               .flat_map(|s| &s.selector_rules)
               .map(|r| r.recipients.len())
               .sum::<usize>() as u32,
       },
   };
   ```
3. Update RLP encode/decode for `KeyAuthorization` (and `SignedKeyAuthorization`)
   to include the new field — see writer
   `crates/primitives/src/transaction/key_authorization.rs` for wire format.

**Estimate**: ~150 lines (struct + RLP + parsing call site).

---

## FU-3 — `SpendingLimitState` 4-field 2-slot storage layout (T3+ periodic limits)

**Why deferred**: leafage's `spending_limits` is
`Mapping<B256, Mapping<Address, U256>>` — a single U256 (`remaining`) per
(account+key, token). Writer's T3+ layout extends this to four fields stored
across two slots:

| Slot | Field         | Type    |
|------|---------------|---------|
| +0   | `remaining`   | `U256`  |
| +1   | packed{ `max` (`u128`), `period` (`u64`), `period_end` (`u64`) } | 1 word |

State diffs from writer write both slots; leafage currently reads only slot
+0, so `remaining` decodes correctly but `max` / `period` / `period_end` are
unreachable.

**Risk**: changing `spending_limits` type cascades to every caller
(`crates/leafage-evm-chains/src/tempo/precompile/account_keychain.rs` lines
355, 414, 470, 551, 557, 588, 590 at time of writing). The pre-T3 wire-protocol
slot+0 layout MUST remain compatible — `remaining` stays in slot +0 as a U256.

**Writer reference**:
`crates/precompiles/src/account_keychain/mod.rs:130-146` (`SpendingLimitState`),
`crates/revm/src/handler.rs:342-348` (T3 periodic-limit gas accounting),
`crates/precompiles/src/account_keychain/mod.rs:350-380` (refund clamp).

**Steps**:
1. Add a `SpendingLimitState` Storable with the exact 2-slot packed layout
   (slot+0 = U256 remaining; slot+1 = packed max + period + period_end).
   Pre-T3 entries that only wrote slot+0 will read back as
   `{remaining: x, max: 0, period: 0, period_end: 0}` — a non-periodic
   limit, which matches writer semantics.
2. Change `spending_limits` from `Mapping<B256, Mapping<Address, U256>>` to
   `Mapping<B256, Mapping<Address, SpendingLimitState>>`.
3. Adapt every caller: existing reads of `remaining` become `.read()?.remaining`;
   writes need to preserve the packed sibling slot on T3+ (read-modify-write).
4. Update `update_spending_limit` to write both slots on T3+ with appropriate
   `max` clamping (T3 caps `max` to TIP-20's `u128` supply range; see writer).

**Estimate**: ~250 lines spread across `account_keychain.rs` + 1 new struct.

**Acceptance**: leafage `getRemainingLimit` returns identical bytes to writer
for the same `(account, keyId, token)` on a post-T3 mainnet block where
periodic limits are configured.

---

## FU-4 — `refund_spending_limit` T3+ clamp to original max

**Depends on**: FU-3 (needs `max` field).

**Why deferred**: without FU-3 there is no `max` to clamp against. Once
`SpendingLimitState` lands, refund accounting on T3+ needs to clamp
`remaining + refund_amount` to `state.max` to prevent saturating_add from
overflowing the configured limit.

**Writer reference**: `crates/precompiles/src/account_keychain/mod.rs:350-380`.

**Steps**: in the existing refund path (in `account_keychain.rs`),
```rust
if spec.is_t3() {
    let new_remaining = remaining.saturating_add(refund_amount).min(state.max);
} else {
    let new_remaining = remaining.saturating_add(refund_amount);
}
```

**Estimate**: ~20 lines + 1 unit test.

---

## FU-5 — `validate_call_scopes` T3 stateful target check

**Depends on**: FU-1.

**Why deferred**: T3 spec requires `validate_call_scopes` to confirm each
target is an *initialized* TIP-20 token (queries TIP20Factory storage). T4
relaxes this to a stateless `target.is_tip20()` address-format check.
Both branches need wiring inside the (currently stub) `set_call_scopes`
implementation; until FU-1 lands, neither is exercised.

**Writer reference**:
`crates/precompiles/src/account_keychain/mod.rs:907-922`
(`validate_selector_rules` with the `if !self.storage.spec().is_t4()`
branch calling `TIP20Factory::is_tip20`).

**Implementation note**: The PR's scaffolding includes the T3/T4 branch
shape inside a no-op `validate_call_scopes` helper structure. Verify that
`TIP20Factory::new().is_tip20(addr)` returns `Result<bool>` matching
writer semantics (TIP-20 address bears the prefix AND has a deployed
token record).

**Estimate**: ~40 lines (validation method) + 4 unit tests (T3 init vs
uninit vs format-only vs T4-bypass).

---

## FU-6 — T3 periodic spending-limit reset logic

**Depends on**: FU-3.

**Why deferred**: T3 introduces periodic limits — when `block.timestamp >=
period_end`, the next spend resets `remaining` to `max` and advances
`period_end` by `period`. Without FU-3 there's no `period_end` to compare
against.

**Writer reference**: `crates/precompiles/src/account_keychain/mod.rs`
spend-limit decrement path (search for `period_end`).

**Steps**: in the spend-limit decrement flow (called from TIP-20 transfer's
fee-deduction path), on T3+:
```rust
let now = self.storage.timestamp();
if state.period > 0 && now >= state.period_end {
    state.remaining = U256::from(state.max);
    let elapsed = now - state.period_end;
    let periods_skipped = elapsed / state.period + 1;
    state.period_end = state.period_end + periods_skipped * state.period;
}
```

**Estimate**: ~60 lines + 3 unit tests (within-window, at boundary,
skipped-periods).

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

**Why deferred**: The PR covers `set_reward_recipient` (mirrors writer
`rewards.rs:139`). Writer `rewards.rs:752` is part of a test, not a
production rejection path, but the writer's `claim_rewards` and any
helper that *changes* the effective reward recipient should also reject
virtual addresses. Audit the leafage rewards-path code against writer
post-T3 to identify any other entry points.

**Files to audit**:
- `crates/leafage-evm-chains/src/tempo/precompile/tip20.rs` — every method
  that touches a `recipient` parameter
- writer `crates/precompiles/src/tip20/rewards.rs` — every `is_virtual()`
  check at T3+ gate

**Estimate**: ~30 lines if any additional sites are found; otherwise nil.

---

## FU-9 — TIP-1038 #1 `setUserToken` read-before-write optimisation

**Why deferred**: pure gas optimisation, doesn't change correctness.
Writer skips the storage write + event emission when `setUserToken` is
called with the same value already stored.

**Writer reference**: `crates/precompiles/src/tip_fee_manager/mod.rs:131`.

**Files**: `crates/leafage-evm-chains/src/tempo/precompile/fee_manager.rs`.

**Estimate**: ~15 lines + 1 unit test.

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
| `eth_call account_keychain.getCallScope(account, keyId)` | **fails until FU-1** | FU-1 |
| `eth_estimateGas` AA tx with only spending limits | byte-identical | PR ✅ |
| `eth_estimateGas` AA tx with call scopes | **under-estimates by `scope_slots * sstore + extra_gas` until FU-2** | FU-2 |
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

## Suggested order for the follow-up PR(s)

1. **FU-1a + FU-1b** (framework prerequisites, both small, isolated)
2. **FU-1** (CallScope storage read/write, the actual user-facing gap)
3. **FU-2** (Envelope parsing — biggest unlock for AA gas accuracy)
4. **FU-3 → FU-4 → FU-6** (Spending limit periodic logic — one cohesive
   storage migration, then refund + reset on top of it)
5. **FU-5** (validate_call_scopes, after FU-1 makes it executable)
6. **FU-8** (rewards audit, small)
7. **FU-9** (gas optimisation, can ship anytime)
8. **FU-10** (regression — run continuously, not a one-shot)
9. **FU-7** (consensus_context — only if business asks)
10. **FU-11** (state gas — only when the writer flag flips)

A single follow-up PR for FU-1a/FU-1b/FU-1/FU-2 covers the
"AA tx with call scopes" gap end-to-end; subsequent PRs can be much
smaller and target one limit-related concern at a time.
