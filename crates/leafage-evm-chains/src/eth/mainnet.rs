//! Ethereum mainnet constants for replaying historical blocks.

use alloy::eips::eip7840::BlobParams;
use alloy::eips::BlobScheduleBlobParams;
use alloy::primitives::Address;
use alloy_hardforks::ethereum::mainnet::{MAINNET_BPO1_TIMESTAMP, MAINNET_BPO2_TIMESTAMP};
use alloy_hardforks::EthereumHardforks;
use std::sync::LazyLock;

pub const ETH_MAINNET_CHAIN_ID: u64 = 1;

/// First block of Cancun: from here on contracts are only destroyed in the
/// transaction creating them (EIP-6780).
pub const ETH_MAINNET_CANCUN_BLOCK: u64 = alloy_hardforks::ethereum::mainnet::MAINNET_CANCUN_BLOCK;

/// The addresses of the mainnet genesis alloc, sorted: 20 bytes each. Only
/// balances, no code, storage or nonces.
static GENESIS_ALLOC: &[u8] = include_bytes!("mainnet_genesis_alloc.bin");

/// The addresses of the mainnet genesis alloc (8893 accounts).
pub fn eth_mainnet_genesis_alloc() -> &'static [Address] {
    static ALLOC: LazyLock<Vec<Address>> = LazyLock::new(|| {
        GENESIS_ALLOC
            .chunks_exact(20)
            .map(Address::from_slice)
            .collect()
    });
    &ALLOC
}

/// The blob parameters of mainnet, with the BPO forks after Osaka.
pub fn eth_mainnet_blob_schedule() -> BlobScheduleBlobParams {
    BlobScheduleBlobParams::mainnet().with_scheduled([
        (MAINNET_BPO1_TIMESTAMP, BlobParams::bpo1()),
        (MAINNET_BPO2_TIMESTAMP, BlobParams::bpo2()),
    ])
}

/// The blob parameters active at `timestamp`, `None` before Cancun.
pub fn eth_mainnet_blob_params_at(
    forks: &impl EthereumHardforks,
    schedule: &BlobScheduleBlobParams,
    timestamp: u64,
) -> Option<BlobParams> {
    if forks.is_osaka_active_at_timestamp(timestamp) {
        Some(
            schedule
                .active_scheduled_params_at_timestamp(timestamp)
                .copied()
                .unwrap_or(schedule.osaka),
        )
    } else if forks.is_prague_active_at_timestamp(timestamp) {
        Some(schedule.prague)
    } else if forks.is_cancun_active_at_timestamp(timestamp) {
        Some(schedule.cancun)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use alloy_hardforks::EthereumChainHardforks;

    #[test]
    fn genesis_alloc() {
        let alloc = eth_mainnet_genesis_alloc();
        assert_eq!(alloc.len(), 8893);
        assert!(alloc.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(alloc.contains(&address!("000d836201318ec6899a67540690382780743280")));
    }

    #[test]
    fn blob_params() {
        let forks = EthereumChainHardforks::mainnet();
        let schedule = eth_mainnet_blob_schedule();
        let at = |ts| eth_mainnet_blob_params_at(&forks, &schedule, ts);
        assert_eq!(at(1_710_338_134), None);
        assert_eq!(at(1_710_338_135), Some(BlobParams::cancun()));
        assert_eq!(at(1_746_612_311), Some(BlobParams::prague()));
        assert_eq!(at(1_764_798_551), Some(BlobParams::osaka()));
        assert_eq!(at(MAINNET_BPO1_TIMESTAMP), Some(BlobParams::bpo1()));
        assert_eq!(at(MAINNET_BPO2_TIMESTAMP + 1), Some(BlobParams::bpo2()));
    }
}
