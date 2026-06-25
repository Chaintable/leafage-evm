//! Base B20 token precompiles — read (view) methods.
//!
//! B20 tokens are precompiles (no deployed bytecode); their state lives in the
//! EVM trie at the token address in an ERC-7201 namespaced layout. This module
//! serves the read surface locally by reading those slots, mirroring Base reth's
//! `b20_asset`/`b20_stablecoin` semantics. See `docs/BaseBerylPrecompiles.md`.
//!
//! The dispatch here is journal-agnostic: it takes an `sload(address, key)`
//! closure so the caller (the `PrecompileProvider` wrapper in the rpc crate)
//! can back it with revm's journal for the op context. Returns the ABI-encoded
//! output (or a revert), or `Err(())` on a storage-read failure.
//!
//! Scope: view methods only (leafage is read-only).

use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::sol_types::{SolCall, SolInterface};

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
// base.b20.stablecoin extension storage root.
const ROOT_STABLECOIN: U256 = U256::from_limbs([
    0xf09e73d0943d6200,
    0x45d0ca58e30b7693,
    0x367ea3129b19441d,
    0x35827975a06ca0e9,
]);

// base.b20 core field slot offsets.
const OFF_NAME: u64 = 0;
const OFF_SYMBOL: u64 = 1;
const OFF_TOTAL_SUPPLY: u64 = 3;
const OFF_BALANCES: u64 = 4;
const OFF_ALLOWANCES: u64 = 5;
const OFF_SUPPLY_CAP: u64 = 12;
// base.b20.asset field slot offsets.
const OFF_ASSET_DECIMALS: u64 = 0; // u8 in slot 0, low byte
const OFF_ASSET_MULTIPLIER: u64 = 1;
// base.b20.stablecoin field slot offsets.
const OFF_STABLECOIN_CURRENCY: u64 = 0; // string

/// WAD fixed-point precision (1e18) for the asset multiplier.
const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
/// Default decimals for an asset token when the slot is unset (Base default).
const ASSET_DEFAULT_DECIMALS: u8 = 6;
/// Fixed decimals for a stablecoin token.
const STABLECOIN_DECIMALS: u8 = 6;

alloy::sol! {
    interface IB20 {
        function balanceOf(address account) external view returns (uint256);
        function totalSupply() external view returns (uint256);
        function decimals() external view returns (uint8);
        function name() external view returns (string);
        function symbol() external view returns (string);
        function allowance(address owner, address spender) external view returns (uint256);
        function supplyCap() external view returns (uint256);
        // Asset-only (b20_asset IB20Asset interface) — must revert on stablecoin:
        function multiplier() external view returns (uint256);
        function scaledBalanceOf(address account) external view returns (uint256);
        function toScaledBalance(uint256 rawBalance) external view returns (uint256);
        function toRawBalance(uint256 scaledBalance) external view returns (uint256);
        function WAD_PRECISION() external view returns (uint256);
        // Stablecoin-only (b20_stablecoin IB20Stablecoin interface) — reverts on asset:
        function currency() external view returns (string);
    }
}

/// Outcome of a B20 view dispatch. `Err(())` (separate) signals a storage-read
/// failure, which the caller maps to a precompile error.
pub enum B20Outcome {
    /// Successful return with ABI-encoded output.
    Return(Bytes),
    /// Revert with ABI-encoded output (e.g. unknown/unsupported selector).
    Revert(Bytes),
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

/// Reads a Solidity `string` storage value at `slot`.
fn read_string<F>(sload: &mut F, slot: U256) -> Result<String, ()>
where
    F: FnMut(U256) -> Result<U256, ()>,
{
    let word = sload(slot)?;
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
            let chunk = sload(base.wrapping_add(U256::from(i)))?.to_be_bytes::<32>();
            let take = (len - out.len()).min(32);
            out.extend_from_slice(&chunk[..take]);
            i += 1;
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

#[inline]
fn read_balance<F>(sload: &mut F, account: Address) -> Result<U256, ()>
where
    F: FnMut(U256) -> Result<U256, ()>,
{
    sload(mapping_slot(field_slot(ROOT_B20, OFF_BALANCES), account.into_word()))
}

#[inline]
fn read_multiplier<F>(sload: &mut F) -> Result<U256, ()>
where
    F: FnMut(U256) -> Result<U256, ()>,
{
    sload(field_slot(ROOT_ASSET, OFF_ASSET_MULTIPLIER))
}

/// Dispatches a single B20 view call against the token's storage.
///
/// `sload(key)` reads storage at the token's address. Returns the ABI-encoded
/// result, or `Err(())` if a storage read fails.
pub fn dispatch<F>(is_asset: bool, data: &[u8], mut sload: F) -> Result<B20Outcome, ()>
where
    F: FnMut(U256) -> Result<U256, ()>,
{
    let revert = Ok(B20Outcome::Revert(Bytes::new()));
    let call = match IB20::IB20Calls::abi_decode(data) {
        Ok(c) => c,
        // Unknown selector -> revert (a token without that method).
        Err(_) => return revert,
    };

    let out: Bytes = match call {
        IB20::IB20Calls::balanceOf(c) => {
            IB20::balanceOfCall::abi_encode_returns(&read_balance(&mut sload, c.account)?).into()
        }
        IB20::IB20Calls::totalSupply(_) => {
            let v = sload(field_slot(ROOT_B20, OFF_TOTAL_SUPPLY))?;
            IB20::totalSupplyCall::abi_encode_returns(&v).into()
        }
        IB20::IB20Calls::supplyCap(_) => {
            let v = sload(field_slot(ROOT_B20, OFF_SUPPLY_CAP))?;
            IB20::supplyCapCall::abi_encode_returns(&v).into()
        }
        IB20::IB20Calls::allowance(c) => {
            let inner = mapping_slot(field_slot(ROOT_B20, OFF_ALLOWANCES), c.owner.into_word());
            let v = sload(mapping_slot(inner, c.spender.into_word()))?;
            IB20::allowanceCall::abi_encode_returns(&v).into()
        }
        IB20::IB20Calls::decimals(_) => {
            let d = if is_asset {
                let word = sload(field_slot(ROOT_ASSET, OFF_ASSET_DECIMALS))?;
                let b = word.to_be_bytes::<32>()[31];
                if b == 0 {
                    ASSET_DEFAULT_DECIMALS
                } else {
                    b
                }
            } else {
                STABLECOIN_DECIMALS
            };
            IB20::decimalsCall::abi_encode_returns(&d).into()
        }
        IB20::IB20Calls::name(_) => {
            let s = read_string(&mut sload, field_slot(ROOT_B20, OFF_NAME))?;
            IB20::nameCall::abi_encode_returns(&s).into()
        }
        IB20::IB20Calls::symbol(_) => {
            let s = read_string(&mut sload, field_slot(ROOT_B20, OFF_SYMBOL))?;
            IB20::symbolCall::abi_encode_returns(&s).into()
        }
        // Stablecoin-only: currency (ISO 4217 code) from the stablecoin extension.
        IB20::IB20Calls::currency(_) if !is_asset => {
            let s = read_string(&mut sload, field_slot(ROOT_STABLECOIN, OFF_STABLECOIN_CURRENCY))?;
            IB20::currencyCall::abi_encode_returns(&s).into()
        }
        // Asset-only methods (WAD_PRECISION + scaled), revert on stablecoin.
        IB20::IB20Calls::WAD_PRECISION(_) if is_asset => {
            IB20::WAD_PRECISIONCall::abi_encode_returns(&WAD).into()
        }
        IB20::IB20Calls::multiplier(_) if is_asset => {
            IB20::multiplierCall::abi_encode_returns(&read_multiplier(&mut sload)?).into()
        }
        IB20::IB20Calls::scaledBalanceOf(c) if is_asset => {
            let raw = read_balance(&mut sload, c.account)?;
            let m = read_multiplier(&mut sload)?;
            let v = raw.checked_mul(m).ok_or(())? / WAD;
            IB20::scaledBalanceOfCall::abi_encode_returns(&v).into()
        }
        IB20::IB20Calls::toScaledBalance(c) if is_asset => {
            let m = read_multiplier(&mut sload)?;
            let v = c.rawBalance.checked_mul(m).ok_or(())? / WAD;
            IB20::toScaledBalanceCall::abi_encode_returns(&v).into()
        }
        IB20::IB20Calls::toRawBalance(c) if is_asset => {
            let m = read_multiplier(&mut sload)?;
            let v = c.scaledBalance.checked_mul(WAD).ok_or(())? / m;
            IB20::toRawBalanceCall::abi_encode_returns(&v).into()
        }
        // Asset-only methods on a stablecoin, currency() on an asset, or any
        // unknown selector -> revert (matches Base's per-variant interfaces).
        _ => return revert,
    };
    Ok(B20Outcome::Return(out))
}

/// The B20 variant discriminant lives in byte 10 of the token address:
/// 0 = asset, 1 = stablecoin (mirrors Base `B20Variant`).
pub fn is_asset_variant(address: &Address) -> bool {
    address.as_slice()[10] == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn erc7201_roots_match_base() {
        assert_eq!(
            format!("{ROOT_B20:#x}"),
            "0xc78b71fee795ddd74aff64ea9b2474194c938c3196430e10bb5f01ed48434000"
        );
        assert_eq!(
            format!("{ROOT_ASSET:#x}"),
            "0xfdc6d4552d1286ade4d9facdbf0fb50d2ec9b89a90e104f26fd277585e374b00"
        );
        assert_eq!(
            format!("{ROOT_STABLECOIN:#x}"),
            "0x35827975a06ca0e9367ea3129b19441d45d0ca58e30b7693f09e73d0943d6200"
        );
    }

    #[test]
    fn wad_is_1e18() {
        assert_eq!(WAD, U256::from(10u64).pow(U256::from(18u64)));
    }

    #[test]
    fn mapping_slot_matches_solidity() {
        let addr = Address::repeat_byte(0x11);
        let base_slot = field_slot(ROOT_B20, OFF_BALANCES);
        let got = mapping_slot(base_slot, addr.into_word());
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(addr.as_slice());
        buf[32..].copy_from_slice(&base_slot.to_be_bytes::<32>());
        assert_eq!(got, U256::from_be_bytes(keccak256(buf).0));
    }

    #[test]
    fn variant_discriminant() {
        let mut a = [0u8; 20];
        a[0] = 0xb2;
        a[10] = 0;
        assert!(is_asset_variant(&Address::from(a)));
        a[10] = 1;
        assert!(!is_asset_variant(&Address::from(a)));
    }

    /// End-to-end dispatch over a mock storage map: balanceOf returns the raw
    /// balance, decimals reads the asset slot, scaledBalanceOf applies the
    /// multiplier.
    #[test]
    fn dispatch_reads_balance_decimals_scaled() {
        let account = Address::repeat_byte(0xAB);
        let mut store: HashMap<U256, U256> = HashMap::new();
        // raw balance = 100
        store.insert(
            mapping_slot(field_slot(ROOT_B20, OFF_BALANCES), account.into_word()),
            U256::from(100u64),
        );
        // decimals = 8
        store.insert(field_slot(ROOT_ASSET, OFF_ASSET_DECIMALS), U256::from(8u64));
        // multiplier = 2 WAD (2x)
        store.insert(field_slot(ROOT_ASSET, OFF_ASSET_MULTIPLIER), WAD * U256::from(2u64));

        let sload = |k: U256| -> Result<U256, ()> { Ok(store.get(&k).copied().unwrap_or_default()) };

        // balanceOf(account) -> raw 100
        let data = IB20::balanceOfCall { account }.abi_encode();
        let out = match dispatch(true, &data, sload).unwrap() {
            B20Outcome::Return(b) => b,
            B20Outcome::Revert(_) => panic!("reverted"),
        };
        let got = IB20::balanceOfCall::abi_decode_returns(&out).unwrap();
        assert_eq!(got, U256::from(100u64));

        // decimals() -> 8
        let data = IB20::decimalsCall {}.abi_encode();
        let out = match dispatch(true, &data, sload).unwrap() {
            B20Outcome::Return(b) => b,
            B20Outcome::Revert(_) => panic!("reverted"),
        };
        assert_eq!(IB20::decimalsCall::abi_decode_returns(&out).unwrap(), 8u8);

        // scaledBalanceOf(account) -> 100 * 2 = 200
        let data = IB20::scaledBalanceOfCall { account }.abi_encode();
        let out = match dispatch(true, &data, sload).unwrap() {
            B20Outcome::Return(b) => b,
            B20Outcome::Revert(_) => panic!("reverted"),
        };
        assert_eq!(
            IB20::scaledBalanceOfCall::abi_decode_returns(&out).unwrap(),
            U256::from(200u64)
        );
    }

    /// Per-variant interface boundaries: a stablecoin serves the shared reads +
    /// `currency()` but reverts the asset-only `WAD_PRECISION`; an asset serves
    /// `WAD_PRECISION` but reverts the stablecoin-only `currency()`.
    #[test]
    fn variant_specific_methods_revert_across_variants() {
        let mut store: HashMap<U256, U256> = HashMap::new();
        // Short Solidity string "USD" at the stablecoin currency slot:
        // bytes[..len]=ascii, last byte = len*2.
        let mut word = [0u8; 32];
        word[..3].copy_from_slice(b"USD");
        word[31] = (3 * 2) as u8;
        store.insert(
            field_slot(ROOT_STABLECOIN, OFF_STABLECOIN_CURRENCY),
            U256::from_be_bytes(word),
        );
        let sload = |k: U256| -> Result<U256, ()> { Ok(store.get(&k).copied().unwrap_or_default()) };

        // Stablecoin (is_asset=false): currency() returns "USD".
        let data = IB20::currencyCall {}.abi_encode();
        let out = match dispatch(false, &data, sload).unwrap() {
            B20Outcome::Return(b) => b,
            B20Outcome::Revert(_) => panic!("stablecoin currency() reverted"),
        };
        assert_eq!(IB20::currencyCall::abi_decode_returns(&out).unwrap(), "USD");

        // Stablecoin: WAD_PRECISION() is asset-only -> revert.
        let data = IB20::WAD_PRECISIONCall {}.abi_encode();
        assert!(matches!(
            dispatch(false, &data, sload).unwrap(),
            B20Outcome::Revert(_)
        ));

        // Asset (is_asset=true): WAD_PRECISION() returns WAD.
        let data = IB20::WAD_PRECISIONCall {}.abi_encode();
        let out = match dispatch(true, &data, sload).unwrap() {
            B20Outcome::Return(b) => b,
            B20Outcome::Revert(_) => panic!("asset WAD_PRECISION() reverted"),
        };
        assert_eq!(IB20::WAD_PRECISIONCall::abi_decode_returns(&out).unwrap(), WAD);

        // Asset: currency() is stablecoin-only -> revert.
        let data = IB20::currencyCall {}.abi_encode();
        assert!(matches!(
            dispatch(true, &data, sload).unwrap(),
            B20Outcome::Revert(_)
        ));
    }
}
