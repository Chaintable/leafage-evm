//! Mainnet application state changes that affect EVM replay across v1.3–v1.5.
use super::{CronosArchiveRead, CronosEra};
use alloy::primitives::{address, keccak256, Address, B256, U256};
use anyhow::{Context as _, Result};
use revm::{database::CacheDB, DatabaseRef};

pub(super) const V13_HEIGHT: u64 = 14_920_000;
const CONTRACT: Address = address!("4a91fe94870Ce48fC0bCb7c51d94677E61783401");
const NEW_VALUE: Address = address!("5BD628a417a65DF2320C7a663ccCF38c5d368f38");

// In cronos/v1.3.0 app/upgrades.go the mainnet handler runs module migrations
// and then SetState(contract, 0, value) before the first transaction.
pub(super) fn apply_upgrade<D: DatabaseRef>(height: u64, db: &mut CacheDB<D>) -> Result<()> {
    if height == V13_HEIGHT {
        db.insert_account_storage(
            CONTRACT,
            U256::ZERO,
            U256::from_be_slice(NEW_VALUE.as_slice()),
        )
        .context(CronosArchiveRead)?;
    }
    Ok(())
}

// The keeper dispatches by topic zero. Match the handlers registered in each
// app/app.go, including the overloaded IBC event from v1.1.0. Consult the
// committed EVM logs before Cosmos runs the hook and can discard the receipt.
pub(super) fn has_native_hook(era: CronosEra, topic: B256) -> bool {
    use std::sync::LazyLock;
    static COMMON: LazyLock<[B256; 3]> = LazyLock::new(|| {
        [
            "__CronosSendToAccount(address,uint256)",
            "__CronosSendToIbc(address,string,uint256)",
            "__CronosSendCroToIbc(address,string,uint256)",
        ]
        .map(keccak256)
    });
    static ETHEREUM: LazyLock<B256> =
        LazyLock::new(|| keccak256("__CronosSendToEthereum(address,uint256,uint256)"));
    static CHAIN: LazyLock<[B256; 2]> = LazyLock::new(|| {
        [
            "__CronosSendToChain(address,address,uint256,uint256,uint256)",
            "__CronosCancelSendToChain(address,uint256)",
        ]
        .map(keccak256)
    });
    static EVM_CHAIN: LazyLock<[B256; 2]> = LazyLock::new(|| {
        [
            "__CronosSendToEvmChain(address,address,uint256,uint256,uint256,bytes)",
            "__CronosCancelSendToEvmChain(address,uint256)",
        ]
        .map(keccak256)
    });
    static IBC_V2: LazyLock<B256> =
        LazyLock::new(|| keccak256("__CronosSendToIbc(address,uint256,string,uint256,bytes)"));

    COMMON.contains(&topic)
        || match era {
            CronosEra::BeforeHuygen | CronosEra::Huygen | CronosEra::HuygenHotfix => {
                topic == *ETHEREUM
            }
            CronosEra::Galileo | CronosEra::FeeMarket => CHAIN.contains(&topic),
            CronosEra::Titan | CronosEra::V12 | CronosEra::V13 => {
                topic == *IBC_V2 || EVM_CHAIN.contains(&topic)
            }
            _ => topic == *IBC_V2,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::{database::EmptyDB, Database};

    #[test]
    fn matches_registered_hooks_at_binary_switches() {
        let older = keccak256("__CronosSendToChain(address,address,uint256,uint256,uint256)");
        let newer = keccak256("__CronosCancelSendToEvmChain(address,uint256)");
        let ibc = keccak256("__CronosSendToIbc(address,uint256,string,uint256,bytes)");
        assert!(has_native_hook(CronosEra::FeeMarket, older));
        assert!(!has_native_hook(CronosEra::Titan, older));
        assert!(!has_native_hook(CronosEra::FeeMarket, ibc));
        assert!(has_native_hook(CronosEra::Titan, ibc));
        assert!(has_native_hook(CronosEra::V13, newer));
        assert!(!has_native_hook(CronosEra::Pallene, newer));
        assert!(has_native_hook(CronosEra::Pallene, ibc));
    }

    #[test]
    fn v13_changes_only_slot_zero_on_the_upgrade_block() {
        // eth_getStorageAt(contract, 0, 14_919_999) on mainnet.
        let old =
            U256::from_be_slice(address!("fe55734cf9f296d216a9d922f9edb1e9696ef347").as_slice());
        for height in [V13_HEIGHT - 1, V13_HEIGHT, V13_HEIGHT + 1] {
            let mut db = CacheDB::new(EmptyDB::default());
            db.insert_account_storage(CONTRACT, U256::ZERO, old)
                .unwrap();
            db.insert_account_storage(CONTRACT, U256::ONE, U256::from(7))
                .unwrap();
            apply_upgrade(height, &mut db).unwrap();
            let expected = if height == V13_HEIGHT {
                U256::from_be_slice(NEW_VALUE.as_slice())
            } else {
                old
            };
            assert_eq!(db.storage(CONTRACT, U256::ZERO).unwrap(), expected);
            assert_eq!(db.storage(CONTRACT, U256::ONE).unwrap(), U256::from(7));
        }
    }
}
