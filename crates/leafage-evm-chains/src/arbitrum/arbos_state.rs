//! Read ArbOS pricing values straight from replica state.
//!
//! Nitro keeps all ArbOS internal state in the storage of one fictional account
//! (`types.ArbosStateAddress`), partitioned into subspaces by a keccak "page"
//! scheme (`arbos/storage/storage.go`):
//!
//! ```text
//! subspace.storageKey = keccak256(parent.storageKey ++ id)   // root key is empty
//! slot(offset)        = keccak256(storageKey ++ key[:31])[:31] ++ key[31]
//!                       where key = uint256(offset) big-endian
//! ```
//!
//! Reading these slots via [`revm::DatabaseRef::storage_ref`] is exactly what
//! `eth_getStorageAt` does (the storage wrapper keccak-hashes both the address
//! and the slot before hitting the DB). The layout is an ArbOS global — it does
//! not depend on chain id / DAC mode / gas token — so one implementation serves
//! every Orbit chain. Offsets can only change across an ArbOS *major* version;
//! the layout-check test in the design doc guards that before enabling a chain.

use once_cell::sync::Lazy;
use revm::primitives::{address, keccak256, Address, U256};
use revm::DatabaseRef;

/// `types.ArbosStateAddress`: the account whose storage holds all ArbOS state.
pub const ARBOS_STATE_ADDRESS: Address = address!("a4b05fffffffffffffffffffffffffffffffffff");

// Subspace ids (`arbos/arbosState/arbosstate.go`).
const L1_PRICING_SUBSPACE: u8 = 0;
const L2_PRICING_SUBSPACE: u8 = 1;

// Offsets within their respective storage spaces.
const PRICE_PER_UNIT_OFFSET: u64 = 7; // L1PricingState
const MIN_BASE_FEE_WEI_OFFSET: u64 = 3; // L2PricingState
const BROTLI_COMPRESSION_LEVEL_OFFSET: u64 = 7; // root ArbOS storage

/// `subspace.storageKey = keccak256(parent.storageKey ++ id)`.
fn subspace_key(parent_key: &[u8], id: u8) -> [u8; 32] {
    keccak256([parent_key, &[id]].concat()).0
}

/// `mapAddress`: keep the low byte of the key verbatim, hash the rest with the
/// storage key. `key = uint256(offset)` big-endian.
fn slot_at(storage_key: &[u8], offset: u64) -> U256 {
    let key = U256::from(offset).to_be_bytes::<32>();
    let mut input = Vec::with_capacity(storage_key.len() + 31);
    input.extend_from_slice(storage_key);
    input.extend_from_slice(&key[..31]);
    let hashed = keccak256(&input).0;
    let mut slot = [0u8; 32];
    slot[..31].copy_from_slice(&hashed[..31]);
    slot[31] = key[31];
    U256::from_be_bytes::<32>(slot)
}

static PRICE_PER_UNIT_SLOT: Lazy<U256> = Lazy::new(|| {
    slot_at(
        &subspace_key(&[], L1_PRICING_SUBSPACE),
        PRICE_PER_UNIT_OFFSET,
    )
});
static MIN_BASE_FEE_SLOT: Lazy<U256> = Lazy::new(|| {
    slot_at(
        &subspace_key(&[], L2_PRICING_SUBSPACE),
        MIN_BASE_FEE_WEI_OFFSET,
    )
});
static BROTLI_LEVEL_SLOT: Lazy<U256> = Lazy::new(|| slot_at(&[], BROTLI_COMPRESSION_LEVEL_OFFSET));

/// The three ArbOS pricing values posterGas estimation needs.
#[derive(Debug, Clone)]
pub struct ArbPricing {
    /// L1 price per calldata unit, in wei (dynamic, updates per block).
    pub price_per_unit: U256,
    /// L2 minimum base fee, in wei (static).
    pub min_base_fee: U256,
    /// Brotli compression level used for L1 posting (static).
    pub brotli_level: u64,
}

/// Read the pricing values from replica state. Returns `None` — meaning no L1
/// overhead, a safe degrade — if any slot read fails or `price_per_unit` is
/// zero (pre-L1-pricing blocks, matching Nitro's early-block semantics).
pub fn read_pricing<S: DatabaseRef>(state: &S) -> Option<ArbPricing> {
    let price_per_unit = state
        .storage_ref(ARBOS_STATE_ADDRESS, *PRICE_PER_UNIT_SLOT)
        .ok()?;
    if price_per_unit.is_zero() {
        return None;
    }
    let min_base_fee = state
        .storage_ref(ARBOS_STATE_ADDRESS, *MIN_BASE_FEE_SLOT)
        .ok()?;
    let brotli_raw = state
        .storage_ref(ARBOS_STATE_ADDRESS, *BROTLI_LEVEL_SLOT)
        .ok()?;
    // Brotli quality is 0..=11; clamp defensively against an unexpected slot value.
    let brotli_level = u64::try_from(brotli_raw).unwrap_or(0).min(11);
    Some(ArbPricing {
        price_per_unit,
        min_base_fee,
        brotli_level,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slots derived here must equal the values read live off the writer
    /// (kava-1, 2026-05-29; see design-doc appendix). These vectors also pin the
    /// subspace ids / offsets for Robinhood's ArbOS version.
    #[test]
    fn slot_derivation_matches_live_vectors() {
        let expect = |hex: &str| U256::from_str_radix(hex, 16).unwrap();
        assert_eq!(
            *PRICE_PER_UNIT_SLOT,
            expect("a9f6f085d78d1d37c5819e5c16c9e03198bd14e08cd1f6f8191bc6207b9e9707"),
            "pricePerUnit slot (L1Pricing subspace, offset 7)"
        );
        assert_eq!(
            *MIN_BASE_FEE_SLOT,
            expect("e54de2a4cdacc0a0059d2b6e16348103df8c4aff409c31e40ec73d11926c8203"),
            "minBaseFeeWei slot (L2Pricing subspace, offset 3)"
        );
        assert_eq!(
            *BROTLI_LEVEL_SLOT,
            expect("15fed0451499512d95f3ec5a41c878b9de55f21878b5b4e190d4667ec709b407"),
            "brotliCompressionLevel slot (root, offset 7)"
        );
    }

    /// The low byte of the offset is carried into the slot verbatim.
    #[test]
    fn slot_keeps_offset_low_byte() {
        let slot = slot_at(&[], 7);
        assert_eq!((slot & U256::from(0xff)).to::<u64>(), 7);
    }
}
