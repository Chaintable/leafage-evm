//! ApeChain native-yield account model, as stored in Chaintable state diffs.
//!
//! ApeChain accounts hold `Flags, Fixed, Shares, Debt, Delegate` instead of a
//! balance (go-ethereum-apechain `core/types/state_account.go`). The Chaintable
//! writer keeps the usual account leaf with balance 0 and writes the raw fields
//! as storage of a reserved account (pipeline `types/ape_yield.go`):
//!
//! ```text
//! logical slot i = keccak256(addrHash ++ uint256(i))
//! slot 0: flags << 248 | delegate (low 20 bytes)
//! slot 1: fixed    slot 2: shares    slot 3: debt
//! ```
//!
//! The balance is `shares * sharePrice + fixed - debt` for automatic yield and
//! `fixed - debt` otherwise; `sharePrice` is ArbOS root storage offset 50.

use crate::primitives::{Address, H256, U256};
use alloy::primitives::{address, b256, keccak256};

/// `"\xa4\xb0" ++ "ape-yield-state" ++ "\x00\x00\x00"`; never a real account.
pub const APE_YIELD_ADDRESS: Address = address!("a4b06170652d7969656c642d7374617465000000");
/// `keccak256(APE_YIELD_ADDRESS)`, the reserved account's key in state diffs.
pub const APE_YIELD_ADDRESS_HASH: H256 =
    b256!("4b316b01bf17c67c525185231fb0cb00499b8351daad6d117524cc9541900087");
pub const APE_YIELD_SLOT_COUNT: usize = 4;

pub const APE_YIELD_AUTOMATIC: u8 = 0;
pub const APE_YIELD_DISABLED: u8 = 1;
pub const APE_YIELD_DELEGATED: u8 = 2;

/// ArbOS root storage offsets of the ApeChain yield state.
pub const APE_SHARE_PRICE_OFFSET: u64 = 50;
pub const APE_SHARE_COUNT_OFFSET: u64 = 51;
pub const APE_APY_OFFSET: u64 = 52;
pub const APE_BASE_SHARE_PRICE_OFFSET: u64 = 53;
pub const APE_BASE_SHARE_TIME_OFFSET: u64 = 54;

/// `types.ArbosStateAddress`.
pub const APE_ARBOS_STATE_ADDRESS: Address = address!("a4b05fffffffffffffffffffffffffffffffffff");

/// Logical slot `i` of the reserved account for the account with `addr_hash`.
pub fn ape_yield_slot(addr_hash: H256, i: usize) -> U256 {
    let mut input = [0u8; 64];
    input[..32].copy_from_slice(addr_hash.as_slice());
    input[63] = i as u8;
    U256::from_be_bytes(keccak256(input).0)
}

/// ArbOS root-storage slot of `offset` (nitro `mapAddress` with an empty key).
pub fn ape_arbos_root_slot(offset: u64) -> U256 {
    let key = U256::from(offset).to_be_bytes::<32>();
    let hashed = keccak256(&key[..31]).0;
    let mut slot = [0u8; 32];
    slot[..31].copy_from_slice(&hashed[..31]);
    slot[31] = key[31];
    U256::from_be_bytes(slot)
}

/// The yield fields of one ApeChain account.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ApeYieldFields {
    pub flags: u8,
    pub fixed: U256,
    pub shares: U256,
    pub debt: U256,
    pub delegate: Address,
}

impl ApeYieldFields {
    pub fn from_slots(slots: [U256; APE_YIELD_SLOT_COUNT]) -> Self {
        let packed = slots[0].to_be_bytes::<32>();
        Self {
            flags: packed[0],
            fixed: slots[1],
            shares: slots[2],
            debt: slots[3],
            delegate: Address::from_slice(&packed[12..]),
        }
    }

    pub fn to_slots(&self) -> [U256; APE_YIELD_SLOT_COUNT] {
        let mut packed = [0u8; 32];
        packed[0] = self.flags;
        packed[12..].copy_from_slice(self.delegate.as_slice());
        [
            U256::from_be_bytes(packed),
            self.fixed,
            self.shares,
            self.debt,
        ]
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `computeShareValue` for automatic yield, `fixed` otherwise.
    pub fn gross(&self, share_price: U256) -> U256 {
        if self.flags == APE_YIELD_AUTOMATIC {
            self.shares
                .wrapping_mul(share_price)
                .wrapping_add(self.fixed)
        } else {
            self.fixed
        }
    }

    /// `stateObject.Balance`. Nitro panics below zero; this saturates.
    pub fn balance(&self, share_price: U256) -> U256 {
        self.gross(share_price).saturating_sub(self.debt)
    }
}

/// `computeSharesAndRemainder`: uint256 `DivMod`, which yields zeros for a
/// zero price.
pub fn ape_shares_and_remainder(share_price: U256, value: U256) -> (U256, U256) {
    if share_price.is_zero() {
        return (U256::ZERO, U256::ZERO);
    }
    (value / share_price, value % share_price)
}

/// Balance of an account from its four reserved-account slot values.
pub fn ape_balance_from_slots(slots: [U256; APE_YIELD_SLOT_COUNT], share_price: U256) -> U256 {
    ApeYieldFields::from_slots(slots).balance(share_price)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        yield_address: Address,
        yield_address_hash: H256,
        accounts: Vec<FixtureAccount>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureAccount {
        address: Address,
        address_hash: H256,
        flags: u8,
        fixed: String,
        shares: String,
        debt: String,
        delegate: Address,
        slots: Vec<H256>,
        storage_keys: Vec<H256>,
        values: Vec<H256>,
    }

    /// Shared with the writer: pipeline `types/testdata/ape_yield_slots.json`.
    #[test]
    fn slot_layout_matches_writer_fixture() {
        let fixture: Fixture =
            serde_json::from_str(include_str!("../testdata/ape_yield_slots.json")).unwrap();
        assert_eq!(fixture.yield_address, APE_YIELD_ADDRESS);
        assert_eq!(fixture.yield_address_hash, APE_YIELD_ADDRESS_HASH);
        assert_eq!(keccak256(APE_YIELD_ADDRESS), APE_YIELD_ADDRESS_HASH);
        for account in fixture.accounts {
            assert_eq!(keccak256(account.address), account.address_hash);
            let fields = ApeYieldFields {
                flags: account.flags,
                fixed: account.fixed.parse().unwrap(),
                shares: account.shares.parse().unwrap(),
                debt: account.debt.parse().unwrap(),
                delegate: account.delegate,
            };
            let slots = fields.to_slots();
            assert_eq!(ApeYieldFields::from_slots(slots), fields);
            for (i, value) in slots.iter().enumerate() {
                let slot = ape_yield_slot(account.address_hash, i);
                assert_eq!(H256::from(slot.to_be_bytes::<32>()), account.slots[i]);
                // State diffs key storage by keccak256(slot), as the StateDB does.
                assert_eq!(keccak256(slot.to_be_bytes::<32>()), account.storage_keys[i]);
                assert_eq!(H256::from(value.to_be_bytes::<32>()), account.values[i]);
            }
        }
    }

    #[test]
    fn balance_follows_yield_mode() {
        let price = U256::from(1_000_000_007u64);
        let automatic = ApeYieldFields {
            fixed: U256::from(5),
            shares: U256::from(3),
            debt: U256::from(2),
            ..Default::default()
        };
        assert_eq!(
            automatic.balance(price),
            U256::from(3_000_000_021u64 + 5 - 2)
        );
        let disabled = ApeYieldFields {
            flags: APE_YIELD_DISABLED,
            ..automatic
        };
        assert_eq!(disabled.balance(price), U256::from(3));
        let underwater = ApeYieldFields {
            debt: U256::from(9),
            ..disabled
        };
        assert_eq!(underwater.balance(price), U256::ZERO);
        assert_eq!(
            ape_shares_and_remainder(U256::ZERO, U256::from(7)),
            (U256::ZERO, U256::ZERO)
        );
    }

    /// ApeChain mainnet: WAPE at a block after the price froze.
    #[test]
    fn mainnet_wape_balance() {
        let fields = ApeYieldFields {
            fixed: U256::from(705_768_859u64),
            shares: U256::from(5_514_621_514_919_413u64),
            ..Default::default()
        };
        let balance = fields.balance(U256::from(1_112_418_342u64));
        assert_eq!(
            balance,
            "6134566122384182378842105".parse::<U256>().unwrap()
        );
    }
}
