//! Blast native-ETH yield: balance derivation for raw yield accounts.
//!
//! Mirrors blast-geth `stateObject.Balance()`: an automatic-yield account is
//! worth `sharePrice * shares + remainder`; every other flags value, including
//! unknown ones, reads `fixed`.

use alloy::primitives::{address, b256, Address, B256, U256};
use leafage_evm_types::BlastAccountExt;

/// Shares predeploy; its storage slot 1 holds the current sharePrice.
pub const BLAST_SHARES_ADDRESS: Address = address!("0x4300000000000000000000000000000000000000");

/// State key of [`BLAST_SHARES_ADDRESS`]: `keccak256(address)`.
pub const BLAST_SHARES_HASH: B256 =
    b256!("0x34ef019b82232cdd8dce3115c0c0787debeb839c4848a6e1df77393f6625ed82");

/// State key of the sharePrice slot: `keccak256(uint256(1))`.
pub const SHARE_PRICE_SLOT_HASH: B256 =
    b256!("0xb10e2d527612073b26eecdfd717e6a320cf44b4afac2b0732d9fcbe2b7fa0cf6");

/// `flags` value of automatic-yield accounts.
pub const YIELD_AUTOMATIC: u8 = 0;

/// Balance of a Blast account. `share_price` reads the sharePrice from the
/// same state view as `ext`; it is only called for automatic-yield accounts.
/// `Ok(None)` if the balance overflows U256, which blast-geth's big.Int
/// arithmetic cannot represent in a U256 either.
pub fn derive_balance<E>(
    ext: &BlastAccountExt,
    share_price: impl FnOnce() -> Result<U256, E>,
) -> Result<Option<U256>, E> {
    if ext.flags != YIELD_AUTOMATIC {
        return Ok(Some(ext.fixed));
    }
    let share_price = share_price()?;
    Ok(share_price
        .checked_mul(ext.shares)
        .and_then(|value| value.checked_add(ext.remainder)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::keccak256;

    fn ext(flags: u8, fixed: u64, shares: u64, remainder: u64) -> BlastAccountExt {
        BlastAccountExt {
            flags,
            fixed: U256::from(fixed),
            shares: U256::from(shares),
            remainder: U256::from(remainder),
        }
    }

    fn at_price(ext: &BlastAccountExt, share_price: U256) -> Option<U256> {
        derive_balance(ext, || Ok::<_, ()>(share_price)).unwrap()
    }

    #[test]
    fn state_keys_match_their_preimages() {
        assert_eq!(BLAST_SHARES_HASH, keccak256(BLAST_SHARES_ADDRESS));
        assert_eq!(
            SHARE_PRICE_SLOT_HASH,
            keccak256(U256::from(1).to_be_bytes::<32>())
        );
    }

    #[test]
    fn automatic_accounts_follow_the_share_price() {
        let account = ext(0, 999, 13, 17);
        assert_eq!(
            at_price(&account, U256::from(5)),
            Some(U256::from(5 * 13 + 17))
        );
        assert_eq!(
            at_price(&account, U256::from(6)),
            Some(U256::from(6 * 13 + 17))
        );
        assert_eq!(at_price(&account, U256::ZERO), Some(U256::from(17)));
    }

    #[test]
    fn other_modes_read_fixed() {
        for flags in [1, 2, 3, 255] {
            assert_eq!(
                at_price(&ext(flags, 42, 13, 17), U256::from(5)),
                Some(U256::from(42))
            );
        }
    }

    /// Fixed-balance accounts never read the sharePrice, so a failing read
    /// cannot fail them.
    #[test]
    fn other_modes_do_not_read_the_share_price() {
        for flags in [1, 2, 255] {
            let balance = derive_balance(&ext(flags, 42, 13, 17), || Err("unreadable"));
            assert_eq!(balance, Ok(Some(U256::from(42))));
        }
        assert_eq!(
            derive_balance(&ext(0, 42, 13, 17), || Err("unreadable")),
            Err("unreadable")
        );
    }

    #[test]
    fn overflow_is_reported() {
        let mut account = ext(0, 0, 0, 1);
        account.shares = U256::MAX;
        assert_eq!(at_price(&account, U256::from(2)), None);
        account.shares = U256::from(1);
        assert_eq!(at_price(&account, U256::MAX), None);
    }
}
