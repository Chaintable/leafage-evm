//! Base B20 token precompiles — read (view) methods.
//!
//! B20 tokens are precompiles (no deployed bytecode); their state lives in the
//! EVM trie at the token address in an ERC-7201 namespaced layout. This module
//! serves the read surface locally by reading those slots via the EVM journal
//! (`EvmInternals::sload`), mirroring Base reth's `b20_asset`/`b20_stablecoin`
//! semantics. See `docs/BaseBerylPrecompiles.md` for the extracted layout.
//!
//! Scope: view methods only (leafage is read-only). Write methods are not
//! reachable through eth_call serving. The registries/factory are forwarded
//! (`-39008`) elsewhere.

use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::sol_types::{SolCall, SolInterface};
use alloy_evm::precompiles::{DynPrecompile, PrecompileLookup, PrecompilesMap};
use alloy_evm::EvmInternals;
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};

use crate::base::precompile::has_b20_prefix;

// ERC-7201 namespace roots (extracted/verified from Base reth).
// base.b20 core storage root.
const ROOT_B20: U256 = U256::from_limbs([
    0xbb5f01ed48434000,
    0x4c938c3196430e10,
    0x4aff64ea9b247419,
    0xc78b71fee795ddd7,
]);
// base.b20.asset extension storage root.
const ROOT_ASSET: U256 = U256::from_limbs([
    0x6fd277585e374b00,
    0x2ec9b89a90e104f2,
    0xe4d9facdbf0fb50d,
    0xfdc6d4552d1286ad,
]);

// base.b20 core field slot offsets.
const OFF_NAME: u64 = 0;
const OFF_SYMBOL: u64 = 1;
const OFF_TOTAL_SUPPLY: u64 = 3;
const OFF_BALANCES: u64 = 4;
const OFF_ALLOWANCES: u64 = 5;
const OFF_SUPPLY_CAP: u64 = 12;
// base.b20.asset field slot offsets.
const OFF_ASSET_DECIMALS: u64 = 0; // u8 in slot 0, byte 0
const OFF_ASSET_MULTIPLIER: u64 = 1;

/// WAD fixed-point precision (1e18) for the asset multiplier.
const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
/// Default decimals for an asset token when the slot is unset (Base default).
const ASSET_DEFAULT_DECIMALS: u8 = 6;
/// Fixed decimals for a stablecoin token.
const STABLECOIN_DECIMALS: u8 = 6;

/// Nominal gas charged per B20 view call (leafage disables gas accounting for
/// reads; this only needs to fit within the call's gas limit).
const B20_VIEW_GAS: u64 = 5_000;

alloy::sol! {
    interface IB20 {
        function balanceOf(address account) external view returns (uint256);
        function totalSupply() external view returns (uint256);
        function decimals() external view returns (uint8);
        function name() external view returns (string);
        function symbol() external view returns (string);
        function allowance(address owner, address spender) external view returns (uint256);
        function supplyCap() external view returns (uint256);
        // Base asset-specific:
        function multiplier() external view returns (uint256);
        function scaledBalanceOf(address account) external view returns (uint256);
        function toScaledBalance(uint256 rawBalance) external view returns (uint256);
        function toRawBalance(uint256 scaledBalance) external view returns (uint256);
        function WAD_PRECISION() external view returns (uint256);
    }
}

#[inline]
fn field_slot(root: U256, offset: u64) -> U256 {
    root.wrapping_add(U256::from(offset))
}

/// Solidity mapping value slot: `keccak256(pad32(key) ++ pad32(slot))`.
#[inline]
fn mapping_slot(slot: U256, key_word: B256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(key_word.as_slice());
    buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

#[inline]
fn db_err() -> PrecompileError {
    PrecompileError::Other("b20 storage read failed".into())
}

#[inline]
fn sload(internals: &mut EvmInternals, token: Address, slot: U256) -> Result<U256, PrecompileError> {
    internals
        .sload(token, slot)
        .map(|loaded| loaded.data)
        .map_err(|_| db_err())
}

/// Reads a Solidity `string`/`bytes` storage value at `slot`.
fn read_string(
    internals: &mut EvmInternals,
    token: Address,
    slot: U256,
) -> Result<String, PrecompileError> {
    let word = sload(internals, token, slot)?;
    let bytes = word.to_be_bytes::<32>();
    let last = bytes[31];
    if last & 1 == 0 {
        // Short string: data in the high bytes, length = last_byte / 2.
        let len = (last / 2) as usize;
        Ok(String::from_utf8_lossy(&bytes[..len]).into_owned())
    } else {
        // Long string: length = (word - 1) / 2; data at keccak256(pad32(slot)).
        let len: usize = ((word - U256::from(1u64)) / U256::from(2u64)).saturating_to();
        let base = U256::from_be_bytes(keccak256(slot.to_be_bytes::<32>()).0);
        let mut out = Vec::with_capacity(len);
        let mut i = 0u64;
        while out.len() < len {
            let chunk = sload(internals, token, base.wrapping_add(U256::from(i)))?
                .to_be_bytes::<32>();
            let take = (len - out.len()).min(32);
            out.extend_from_slice(&chunk[..take]);
            i += 1;
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

#[inline]
fn balance_of(internals: &mut EvmInternals, token: Address, account: Address) -> Result<U256, PrecompileError> {
    let slot = mapping_slot(field_slot(ROOT_B20, OFF_BALANCES), account.into_word());
    sload(internals, token, slot)
}

#[inline]
fn multiplier(internals: &mut EvmInternals, token: Address) -> Result<U256, PrecompileError> {
    sload(internals, token, field_slot(ROOT_ASSET, OFF_ASSET_MULTIPLIER))
}

fn ok(bytes: Bytes) -> PrecompileResult {
    Ok(PrecompileOutput::new(B20_VIEW_GAS, bytes))
}

/// Dispatches a single B20 view call against the token's storage.
fn dispatch(
    internals: &mut EvmInternals,
    token: Address,
    is_asset: bool,
    data: &[u8],
) -> PrecompileResult {
    let call = match IB20::IB20Calls::abi_decode(data) {
        Ok(c) => c,
        // Unknown selector -> empty revert (matches a token without that method).
        Err(_) => return Ok(PrecompileOutput::new_reverted(0, Bytes::new())),
    };

    match call {
        IB20::IB20Calls::balanceOf(c) => {
            let v = balance_of(internals, token, c.account)?;
            ok(IB20::balanceOfCall::abi_encode_returns(&v).into())
        }
        IB20::IB20Calls::totalSupply(_) => {
            let v = sload(internals, token, field_slot(ROOT_B20, OFF_TOTAL_SUPPLY))?;
            ok(IB20::totalSupplyCall::abi_encode_returns(&v).into())
        }
        IB20::IB20Calls::supplyCap(_) => {
            let v = sload(internals, token, field_slot(ROOT_B20, OFF_SUPPLY_CAP))?;
            ok(IB20::supplyCapCall::abi_encode_returns(&v).into())
        }
        IB20::IB20Calls::allowance(c) => {
            let inner = mapping_slot(field_slot(ROOT_B20, OFF_ALLOWANCES), c.owner.into_word());
            let slot = mapping_slot(inner, c.spender.into_word());
            let v = sload(internals, token, slot)?;
            ok(IB20::allowanceCall::abi_encode_returns(&v).into())
        }
        IB20::IB20Calls::decimals(_) => {
            let d = if is_asset {
                let word = sload(internals, token, field_slot(ROOT_ASSET, OFF_ASSET_DECIMALS))?;
                let b = word.to_be_bytes::<32>()[31];
                if b == 0 {
                    ASSET_DEFAULT_DECIMALS
                } else {
                    b
                }
            } else {
                STABLECOIN_DECIMALS
            };
            ok(IB20::decimalsCall::abi_encode_returns(&d).into())
        }
        IB20::IB20Calls::name(_) => {
            let s = read_string(internals, token, field_slot(ROOT_B20, OFF_NAME))?;
            ok(IB20::nameCall::abi_encode_returns(&s).into())
        }
        IB20::IB20Calls::symbol(_) => {
            let s = read_string(internals, token, field_slot(ROOT_B20, OFF_SYMBOL))?;
            ok(IB20::symbolCall::abi_encode_returns(&s).into())
        }
        IB20::IB20Calls::WAD_PRECISION(_) => {
            ok(IB20::WAD_PRECISIONCall::abi_encode_returns(&WAD).into())
        }
        // Asset-only scaled methods (raw * multiplier / WAD). On a stablecoin
        // these don't exist; revert.
        IB20::IB20Calls::multiplier(_) if is_asset => {
            let v = multiplier(internals, token)?;
            ok(IB20::multiplierCall::abi_encode_returns(&v).into())
        }
        IB20::IB20Calls::scaledBalanceOf(c) if is_asset => {
            let raw = balance_of(internals, token, c.account)?;
            let m = multiplier(internals, token)?;
            let v = raw.checked_mul(m).ok_or_else(db_err)? / WAD;
            ok(IB20::scaledBalanceOfCall::abi_encode_returns(&v).into())
        }
        IB20::IB20Calls::toScaledBalance(c) if is_asset => {
            let m = multiplier(internals, token)?;
            let v = c.rawBalance.checked_mul(m).ok_or_else(db_err)? / WAD;
            ok(IB20::toScaledBalanceCall::abi_encode_returns(&v).into())
        }
        IB20::IB20Calls::toRawBalance(c) if is_asset => {
            let m = multiplier(internals, token)?;
            let v = c.scaledBalance.checked_mul(WAD).ok_or_else(db_err)? / m;
            ok(IB20::toRawBalanceCall::abi_encode_returns(&v).into())
        }
        // scaled methods on a non-asset token, or anything else -> revert.
        _ => Ok(PrecompileOutput::new_reverted(0, Bytes::new())),
    }
}

/// Builds the `DynPrecompile` for a B20 token at `token` (asset or stablecoin).
fn create_b20_precompile(token: Address, is_asset: bool) -> DynPrecompile {
    DynPrecompile::new_stateful(
        PrecompileId::Custom("base-b20".into()),
        move |input| {
            let data = input.data;
            let mut internals = input.internals;
            dispatch(&mut internals, token, is_asset, data)
        },
    )
}

/// The B20 variant discriminant lives in byte 10 of the token address.
/// 0 = asset, 1 = stablecoin (mirrors Base `B20Variant`).
fn is_asset_variant(address: &Address) -> bool {
    address.as_slice()[10] == 0
}

/// Installs the Beryl B20-token dynamic prefix lookup into `precompiles`:
/// any `0xb2_00…00_<variant>` address is dispatched to the B20 read precompile.
pub fn extend_base_precompiles(precompiles: &mut PrecompilesMap) {
    precompiles.set_precompile_lookup(move |address: &Address| {
        if has_b20_prefix(address) {
            Some(create_b20_precompile(*address, is_asset_variant(address)))
        } else {
            None
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erc7201_roots_match_base() {
        // Roots verified against Base reth core_storage.rs / b20_asset/storage.rs.
        assert_eq!(
            format!("{ROOT_B20:#x}"),
            "0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000"
        );
        assert_eq!(
            format!("{ROOT_ASSET:#x}"),
            "0xfdc6d4552d1286ade4d9facdbf0fb50d2ec9b89a90e104f26fd277585e374b00"
        );
    }

    #[test]
    fn wad_is_1e18() {
        assert_eq!(WAD, U256::from(10u64).pow(U256::from(18u64)));
    }

    #[test]
    fn mapping_slot_matches_solidity() {
        // balances[addr] at base slot S: keccak256(pad32(addr) ++ pad32(S)).
        let addr = Address::repeat_byte(0x11);
        let base_slot = field_slot(ROOT_B20, OFF_BALANCES);
        let got = mapping_slot(base_slot, addr.into_word());
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(addr.as_slice());
        buf[32..].copy_from_slice(&base_slot.to_be_bytes::<32>());
        let want = U256::from_be_bytes(keccak256(buf).0);
        assert_eq!(got, want);
    }

    #[test]
    fn variant_discriminant() {
        // B20 address = 0xb2 ++ [0;9] ++ variant(1) ++ tail(9). byte 10 is the
        // variant discriminant: 0 = asset, 1 = stablecoin.
        let mut asset_bytes = [0u8; 20];
        asset_bytes[0] = 0xb2;
        asset_bytes[10] = 0;
        let asset = Address::from(asset_bytes);

        let mut stable_bytes = [0u8; 20];
        stable_bytes[0] = 0xb2;
        stable_bytes[10] = 1;
        let stable = Address::from(stable_bytes);

        assert!(has_b20_prefix(&asset) && has_b20_prefix(&stable));
        assert!(is_asset_variant(&asset));
        assert!(!is_asset_variant(&stable));
    }
}
