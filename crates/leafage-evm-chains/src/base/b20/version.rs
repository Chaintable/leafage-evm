//! Hardfork routing for the B20 precompiles.
//!
//! Base resolves the active token logic per block from the hardfork
//! (`b20_asset/versions.rs`, `b20_stablecoin/versions.rs`, `policy/versions.rs`,
//! `spec.rs::UpgradeGatedStorageFeatures`): V1 from Beryl, V2 from Cobalt. Token addresses do
//! not change — the same state is read by different logic — so leafage must pick the version
//! from the executing block, not from the token.
//!
//! Base gates the persistent-storage features (dynamic string tail cleanup, compact ABI decode
//! errors) on the same fork, so a single [`B20Version`] covers both.

use alloy::primitives::U256;

/// Base mainnet chain ID.
pub const BASE_MAINNET_CHAIN_ID: u64 = 8453;
/// Base Sepolia chain ID.
pub const BASE_SEPOLIA_CHAIN_ID: u64 = 84532;

/// Cobalt activation on Base mainnet: 2026-09-30 18:00:00 UTC.
///
/// From Base reth v1.4.2 `crates/common/chains/src/config.rs` (`cobalt_timestamp`).
pub const BASE_MAINNET_COBALT_TIMESTAMP: u64 = 1_790_791_200;
/// Cobalt activation on Base Sepolia: 2026-09-23 18:00:00 UTC.
pub const BASE_SEPOLIA_COBALT_TIMESTAMP: u64 = 1_790_186_400;

/// The B20 logic version active for a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum B20Version {
    /// Beryl: the first native B20 surface.
    V1,
    /// Cobalt: seize, the ERC-8056 scheduled multiplier, composite policies, metered permit,
    /// and the Cobalt storage features.
    V2,
}

impl B20Version {
    /// The Cobalt activation timestamp for `chain_id`, if Cobalt is scheduled there.
    pub const fn cobalt_timestamp(chain_id: u64) -> Option<u64> {
        match chain_id {
            BASE_MAINNET_CHAIN_ID => Some(BASE_MAINNET_COBALT_TIMESTAMP),
            BASE_SEPOLIA_CHAIN_ID => Some(BASE_SEPOLIA_COBALT_TIMESTAMP),
            _ => None,
        }
    }

    /// The version active on `chain_id` at block `timestamp`.
    ///
    /// Cobalt activates at `timestamp >= cobalt_timestamp`, matching Base's
    /// `ForkCondition::Timestamp`. A chain without a Cobalt schedule stays on V1.
    pub fn resolve(chain_id: u64, timestamp: U256) -> Self {
        match Self::cobalt_timestamp(chain_id) {
            Some(cobalt) if timestamp >= U256::from(cobalt) => Self::V2,
            _ => Self::V1,
        }
    }

    /// Whether Cobalt-era logic applies.
    pub fn is_cobalt(self) -> bool {
        self >= Self::V2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boundaries pinned by Base's `is_cobalt_active_at_timestamp` tests.
    #[test]
    fn cobalt_activates_at_the_scheduled_second() {
        let at = |chain, ts: u64| B20Version::resolve(chain, U256::from(ts));
        assert_eq!(at(BASE_MAINNET_CHAIN_ID, 1_790_791_199), B20Version::V1);
        assert_eq!(at(BASE_MAINNET_CHAIN_ID, 1_790_791_200), B20Version::V2);
        assert_eq!(at(BASE_SEPOLIA_CHAIN_ID, 1_790_186_399), B20Version::V1);
        assert_eq!(at(BASE_SEPOLIA_CHAIN_ID, 1_790_186_400), B20Version::V2);
    }

    #[test]
    fn unscheduled_chains_stay_on_v1() {
        assert_eq!(B20Version::resolve(1, U256::MAX), B20Version::V1);
    }
}
