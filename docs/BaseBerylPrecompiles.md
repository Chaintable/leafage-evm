# Base Beryl Precompiles — port spec (Stage 2)

This documents the exact on-disk layout and read semantics for Base's Beryl
B20-token precompiles, extracted from Base reth (`/Users/cifer/base`,
`crates/common/precompiles`). It is the correctness foundation for the leafage
local port (`--evm-type=base`). **Validate any implementation against a real
Base node before trusting it.**

## Background

Base (from the Beryl upgrade) exposes B20 tokens as **precompiles**, not deployed
contracts: there is no EVM bytecode at a B20 address; the "code" is Rust,
dispatched dynamically by address prefix (`BerylLookup`). The **state**, however,
lives in the EVM trie at the token address, in an **ERC-7201 namespaced** layout.
So leafage can serve the reads locally by reading those slots — but the plain op
EVM treats the address as empty and must be taught to dispatch it.

## Address scheme

- B20 token: any address with first byte `0xb2` and bytes `[1..10] == 0`
  (`0xb2_00…00_<variant>`); byte 10 is the variant discriminant
  (asset/stablecoin). Detector: `base::precompile::has_b20_prefix`.
- Registries (forwarded as `-39008` in Stage 3): B20Factory `0xB20F…0000`,
  ActivationRegistry `0x8453…0001`, PolicyRegistry `0x8453…0002`.

## Storage layout (ERC-7201)

Each namespace's struct is laid out sequentially starting **at** the namespace
root: `field_slot = ROOT + offset_slots` (256-bit wrapping add).

### `base.b20` core — `ROOT_B20 = 0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000`

| offset | field | type |
| --- | --- | --- |
| 0 | name | string |
| 1 | symbol | string |
| 2 | contract_uri | string |
| 3 | total_supply | uint256 |
| 4 | balances | mapping(address → uint256) |
| 5 | allowances | mapping(address → mapping(address → uint256)) |
| 6 | roles | mapping(bytes32 → mapping(address → bool)) |
| 7 | role_admins | mapping(bytes32 → bytes32) |
| 8 | admin_count | uint256 |
| 9 | transfer_*_policy_id | 3× u64 packed (bytes 0/8/16) |
| 10 | mint_receiver_policy_id | u64 (byte 0) |
| 11 | paused | uint256 |
| 12 | supply_cap | uint256 |
| 13 | nonces | mapping(address → uint256) |

### `base.b20.asset` extension — `ROOT_ASSET = 0xfdc6d4552d1286ade4d9facdbf0fb50d2ec9b89a90e104f26fd277585e374b00`

| offset | field | type |
| --- | --- | --- |
| 0 (byte 0) | decimals | u8 (default 6 if unset) |
| 1 | multiplier | uint256 (WAD = 1e18) |
| 2 | used_announcement_ids | mapping |
| 3 | extra_metadata | mapping |

(Stablecoin extension `base.b20.stablecoin` is analogous; decimals fixed at 6.)

### Slot derivation (standard Solidity)

- mapping value: `keccak256(pad32(key) ++ pad32(slot))`.
- nested mapping (allowance): `keccak256(pad32(spender) ++ pad32(keccak256(pad32(owner) ++ pad32(ROOT_B20+5))))`.
- string: short (len < 32) → bytes packed in the slot, `len = (slot[31] / 2)`;
  long → slot holds `2*len+1`, data at `keccak256(pad32(slot))…`.

## Read semantics — the important part

The **standard ERC-20 view methods return RAW stored values** (no multiplier):

- `balanceOf(account)` → raw `balances[account]` (`dispatch.rs:145`).
- `totalSupply()` → raw `total_supply` (`dispatch.rs:144`).
- `allowance(owner, spender)` → raw nested mapping value.
- `decimals()` → asset: `ROOT_ASSET` slot 0 low byte (default 6); stablecoin: 6.
- `name()` / `symbol()` → Solidity string decode at `ROOT_B20 + 0/1`.

The WAD multiplier is applied **only** by Base-specific methods
(`scaledBalanceOf`, `toScaledBalance` = `raw * multiplier / 1e18`,
`toRawBalance`, `multiplier`, `WAD_PRECISION`), not by the ERC-20 surface. So the
common read path is a plain ERC-20 read over the namespaced layout.

## Implementation plan (leafage, revm 36)

1. `PrecompileStorageProvider` adapter: `sload(addr, key)` over leafage's
   `StateDB` (the EVM journal/state), used by the read methods.
2. A `DynPrecompile` for B20 asset + stablecoin implementing the ERC-20 view
   selectors above (and the asset scaled methods), reading the slots per this
   spec. Dispatch by 4-byte selector.
3. Wire a `PrecompilesMap` in `create_base_evm_from_state`:
   start from the op precompile set, then `set_precompile_lookup` with a
   `BerylLookup`-equivalent that returns the B20 precompile for `has_b20_prefix`
   addresses.
4. Stage 3: registries added to the unsupported set → `-39008`.

## Status: full port (reads + writes, metered gas)

Stages 2 and 3 above are superseded. `crates/leafage-evm-chains/src/base/b20/` now ports
the whole token surface — every `IB20` selector plus the asset and stablecoin extensions,
mutations included — and meters gas per storage access instead of charging a flat fee.

Why a port and not a dependency on `base-common-precompiles`: that crate is built against
revm 40 / alloy-evm 0.36, leafage against revm 36 / alloy-evm 0.29, and no published
`op-revm` supports revm 40 (the latest, 20.0.0, requires revm ^38) while leafage's entire
Base path is built on `op_revm::OpEvm`. Linking it would mean a four-major revm bump plus
replacing op-revm across every OP-stack chain leafage supports. The port is confined to two
modules instead.

Still forwarded as `-39008` (`is_forwarded_registry`): the B20 factory, the activation
registry, and the policy registry's *administrative* dispatch. The policy registry's read
path is ported, because every transfer consults it.

### Gas semantics worth knowing

Two things are easy to get wrong and are pinned by tests:

- **Mapping-slot keccak is not metered.** Base charges keccak gas only for the factory's
  address derivation, never for `keccak256(key ++ slot)`. Slot math is free; only the
  resulting SLOAD/SSTORE costs.
- **An `SSTORE` requires more than the 2300 call stipend to *remain*, though it does not
  spend it** (EIP-2200). A call whose last write is followed by little work is bounded by
  that reserve rather than by its own total. `transfer` spends 10,574 but requires 11,019.
  Summing charges without modelling the reserve under-reports by 445.

## Validation

`crates/leafage-evm-chains/tests/b20_gas.rs` pins the port against Base mainnet. Reference
numbers were obtained by binary-searching `eth_call`'s `gas` parameter for the smallest
limit that does not run out, minus the intrinsic cost — i.e. the exact gas each call needs.
Eleven cases match to the gas, and each primitive is pinned independently:

| call | Base mainnet | what it pins |
| --- | --- | --- |
| `DEFAULT_ADMIN_ROLE()` | 106 | calldata (6/word) + init account read (100) |
| `balanceOf` / `policyId` | 2212 | cold SLOAD = 2100 |
| `transfer -> 0x0` | 2218 | pause read, then revert |
| `transfer` (insufficient) | 6518 | both policy checks + sender balance read |
| `approve(0)` | 4074 | first-touch no-op SSTORE 2200 + LOG 1756 |
| `approve(1)` | 23974 | real 0→1 SSTORE: +19,900 |
| `transfer` | 11019 | the stipend reserve |
| `transfer` to self | 9019 | recipient read warm: −2000 |
| `transferWithMemo` | 12080 | extra LOG lifts it off the reserve |
| `transferFrom` | 14981 | allowance write-back is the last SSTORE |

Re-run those probes against a real Base node after any change to the charge sequence in
`ops.rs` or `layout.rs`; the ordering of guards decides both the error a failing call
reports and the gas a succeeding one costs.

## References

- Base reth: `crates/common/precompiles/src/{common/core_storage.rs,common/ops/*,b20_asset/*,b20_stablecoin/*,policy/*,lookup.rs,provider.rs}`,
  `crates/common/precompile-storage/src/evm.rs` (the metering model)
- leafage: `crates/leafage-evm-chains/src/base/b20/`,
  `crates/leafage-evm-rpc/src/api_impl/base/{metered.rs,precompiles.rs}`
