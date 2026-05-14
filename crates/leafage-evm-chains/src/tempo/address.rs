//! Tempo address helpers (TIP-20 prefix detection, TIP-1022 virtual addresses).
//!
//! Ported from Tempo writer: `crates/primitives/src/address.rs`.

use alloy::primitives::{hex, Address, FixedBytes};

/// TIP-20 token address prefix (12 bytes).
///
/// The full TIP-20 address layout is: `TIP20_TOKEN_PREFIX (12 bytes) || derived_bytes (8 bytes)`.
const TIP20_TOKEN_PREFIX: [u8; 12] = hex!("20C000000000000000000000");

/// Returns `true` if `addr` has the TIP-20 token prefix.
///
/// NOTE: This only checks the prefix, not whether the token was actually created.
/// Use `TIP20Factory::is_tip20()` for full validation.
pub fn is_tip20_prefix(addr: Address) -> bool {
    addr.as_slice().starts_with(&TIP20_TOKEN_PREFIX)
}

/// 4-byte master identifier derived from the registration hash.
pub type MasterId = FixedBytes<4>;

/// 6-byte user tag occupying the trailing bytes of a virtual address.
pub type UserTag = FixedBytes<6>;

/// Extension trait with helper functions for Tempo addresses.
pub trait TempoAddressExt {
    /// 12-byte prefix shared by all TIP-20 token addresses.
    ///
    /// NOTE: prefix alone does not prove a token exists — use `TIP20Factory::is_tip20()` for that.
    const TIP20_PREFIX: [u8; 12];

    /// 10-byte magic value occupying bytes `[4:14]` of every TIP-1022 virtual address.
    const VIRTUAL_MAGIC: [u8; 10];

    /// Returns `true` if the address has the TIP-20 token prefix.
    fn is_tip20(&self) -> bool;

    /// Returns `true` if the address matches the TIP-1022 virtual-address format
    /// (bytes `[4:14]` == [`Self::VIRTUAL_MAGIC`]).
    fn is_virtual(&self) -> bool;

    /// Returns `true` if the address is eligible to be a virtual-address master per TIP-1022.
    fn is_valid_master(&self) -> bool;

    /// Decodes a virtual address into its `(masterId, userTag)` components.
    ///
    /// Returns `None` if the address does not match the virtual-address format.
    fn decode_virtual(&self) -> Option<(MasterId, UserTag)>;

    /// Builds a TIP-1022 virtual address from a `masterId` and `userTag`.
    fn new_virtual(master_id: MasterId, user_tag: UserTag) -> Self;
}

impl TempoAddressExt for Address {
    const TIP20_PREFIX: [u8; 12] = TIP20_TOKEN_PREFIX;
    const VIRTUAL_MAGIC: [u8; 10] = [0xFD; 10];

    fn is_tip20(&self) -> bool {
        is_tip20_prefix(*self)
    }

    fn is_virtual(&self) -> bool {
        self.as_slice()[4..14] == Self::VIRTUAL_MAGIC
    }

    fn is_valid_master(&self) -> bool {
        !self.is_zero() && !self.is_virtual() && !self.is_tip20()
    }

    fn decode_virtual(&self) -> Option<(MasterId, UserTag)> {
        if !self.is_virtual() {
            return None;
        }
        let bytes = self.as_slice();
        Some((
            MasterId::from_slice(&bytes[0..4]),
            UserTag::from_slice(&bytes[14..20]),
        ))
    }

    fn new_virtual(master_id: MasterId, user_tag: UserTag) -> Self {
        let mut bytes = [0u8; 20];
        bytes[0..4].copy_from_slice(master_id.as_slice());
        bytes[4..14].copy_from_slice(&Self::VIRTUAL_MAGIC);
        bytes[14..20].copy_from_slice(user_tag.as_slice());
        Self::from(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn virtual_magic_occupies_bytes_4_to_14() {
        let master_id = MasterId::from_slice(&[0x12, 0x34, 0x56, 0x78]);
        let user_tag = UserTag::from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        let addr = Address::new_virtual(master_id, user_tag);

        let bytes = addr.as_slice();
        assert_eq!(&bytes[0..4], master_id.as_slice());
        assert_eq!(&bytes[4..14], &[0xFD; 10]);
        assert_eq!(&bytes[14..20], user_tag.as_slice());
    }

    #[test]
    fn decode_virtual_round_trip() {
        let master_id = MasterId::from_slice(&[1, 2, 3, 4]);
        let user_tag = UserTag::from_slice(&[5, 6, 7, 8, 9, 10]);
        let addr = Address::new_virtual(master_id, user_tag);

        assert!(addr.is_virtual());
        let (m, u) = addr.decode_virtual().expect("virtual address decodes");
        assert_eq!(m, master_id);
        assert_eq!(u, user_tag);
    }

    #[test]
    fn non_virtual_decode_returns_none() {
        let eoa = address!("0x1234567890123456789012345678901234567890");
        assert!(!eoa.is_virtual());
        assert!(eoa.decode_virtual().is_none());
    }

    #[test]
    fn is_virtual_rejects_one_byte_off() {
        // bytes[4..14] must be exactly 0xFD * 10. Flip one byte.
        let mut bytes = [0u8; 20];
        bytes[4..14].copy_from_slice(&[0xFD; 10]);
        bytes[7] = 0xFE; // one-byte deviation
        let addr = Address::from(bytes);
        assert!(!addr.is_virtual());
    }

    #[test]
    fn tip20_prefix_detection() {
        let tip20 = address!("0x20C0000000000000000000000123456789ABCDEF");
        assert!(tip20.is_tip20());
        assert!(is_tip20_prefix(tip20));

        let eoa = address!("0x1234567890123456789012345678901234567890");
        assert!(!eoa.is_tip20());

        // Path-USD (the canonical TIP-20 fee token) shares the prefix.
        let path_usd = address!("0x20C0000000000000000000000000000000000000");
        assert!(path_usd.is_tip20());
    }

    #[test]
    fn is_valid_master_rejects_zero_virtual_tip20() {
        // Zero address rejected.
        assert!(!Address::ZERO.is_valid_master());

        // Virtual address rejected.
        let virt = Address::new_virtual(
            MasterId::from_slice(&[0; 4]),
            UserTag::from_slice(&[0; 6]),
        );
        assert!(!virt.is_valid_master());

        // TIP-20 prefix rejected.
        let tip20 = address!("0x20C0000000000000000000000000000000000001");
        assert!(!tip20.is_valid_master());

        // Plain EOA accepted.
        let eoa = address!("0x1234567890123456789012345678901234567890");
        assert!(eoa.is_valid_master());
    }
}
