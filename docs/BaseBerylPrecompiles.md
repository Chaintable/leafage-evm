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

## Validation (required)

For several known B20 token addresses on Base, compare leafage's
`balanceOf`/`totalSupply`/`decimals`/`name`/`symbol`/`allowance`/`scaledBalanceOf`
against a real Base node at the same block. Do not ship without this.

## References

- Base reth: `crates/common/precompiles/src/{common/core_storage.rs,b20_asset/*,b20_stablecoin/*,lookup.rs,provider.rs}`
- leafage: `crates/leafage-evm-chains/src/base/precompile.rs`, `crates/leafage-evm-rpc/src/api_impl/base/`
