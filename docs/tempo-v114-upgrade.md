# Leafage formal Tempo T11 upgrade

2026-09-08. Protocol reference: official Tempo v1.14.0, `1ec5653e39c97e02807dddf5e7106e979f3ad909`. Builds on PR #150 at `04f75a1f5107186bfa59e0e3190df9aa6460d149`.

## Scope

- Mainnet T11 activates at timestamp 1789048800; historical routing remains timestamp-based and legacy Default remains T10.
- Alloy Core nine-package set moves from the memory-limit backport at git1.6.1 to registry1.7.2. All Tempo ABI dispatchers use strict decoding from T11, retaining the 16 MiB memory cap at every fork.
- Withdrawn TIP-1099 RLP selector and T11 authorization-disable gates are removed. Formal ABI authorizations and setAllowedCalls retain their historical schedules; withdrawn selectors remain negative fixtures.
- Duplicate validation charges 20 gas per inspected item before sorting from T11, with checked arithmetic and upstream validation order. ZoneFactory rejects duplicates both within and across role lists.
- REVM36, alloy-evm0.29.2 and Alloy consensus1.8.2 are unchanged. Cargo also re-resolved existing Windows dependency edges and prost-build's heck edge, without adding further package versions.

Official Tempo contracts/hardfork dependencies are a subsequent independent PR. This change does not introduce the full SDK or change the generic execution engine.

## Validation

Rust/Cargo1.96.1, macOS arm64. Raw logs: `target/v114-validation/`.

- Locked chains/rpc/types check passed, final recheck 5.14s.
- Tempo final suite: 332 passed. Includes strict ABI trailing/overlap/gap/padding, memory cap, exact duplicate gas/OOG order, withdrawn selector negatives, retained authorization positives, and same-storage T10→T11 nonce replay/capacity/expiry checks.
- Full chains suite before the final nonce test: 648 passed, 4 existing ignored. Types: 21 passed.
- Full RPC library: 80 passed, 1 failed. The unchanged generic cancellation test asserts exactly five iterations in 50ms and observed four; isolated retest passed, complete suites reproduced the failure. It is not removed, skipped or counted as passing.
- A new real JSON-RPC test runs 64 eth_call/estimateGas combinations across activation−1/activation/activation+1/return-to-history, with and without timestamp overrides, and canonical/noncanonical inputs. It preserves exact existing error formats: eth_call `-32603 / Reverted: ""`; estimateGas `-39000 / empty message`.
- RPC integration targets: arbitrum_retryable_estimate1, blockx_batch3, blockx_wire_contract7, e2e_smoke2; all 13 passed.
- Changed Rust regions were formatted; global formatting still reports pre-existing unrelated differences. No warning-free/global-format PASS is claimed.

## Pending

CI/images, independent formal-T11 oracle comparisons, data/pipeline rehearsal and a fresh 24-hour fixed-version observation remain pending. The in-memory nonce transition test is not a persisted-database migration or full pipeline test. Real T11 mainnet activation cannot be validated in advance.

Previously excluded storage-wipe consumption, pre_traceMany initial-state conventions, root trace metadata, nested event addresses, native storage flags and generic cancellation/profiling remain unchanged.
