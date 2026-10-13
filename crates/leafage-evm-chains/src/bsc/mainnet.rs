//! BSC mainnet hardfork schedule, mirroring go-bsc `params.BSCChainConfig`.
//!
//! The live RPC path runs with a single configured spec; replaying historical
//! blocks needs the spec that was active at each height instead.

use crate::bsc::hardforks::{bsc::BscHardfork, BscHardforks};
use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};
use revm::primitives::{address, Address};

pub const BSC_MAINNET_CHAIN_ID: u64 = 56;

/// The accounts of the BSC mainnet genesis alloc (`bscMainnetAllocData`):
/// the system contracts, the zero-balance coinbase stand-in and two funded
/// accounts. None has storage.
pub const BSC_MAINNET_GENESIS_ALLOC: [Address; 13] = [
    address!("ffffFFFfFFffffffffffffffFfFFFfffFFFfFFfE"),
    address!("0000000000000000000000000000000000001000"),
    address!("0000000000000000000000000000000000001001"),
    address!("0000000000000000000000000000000000001002"),
    address!("0000000000000000000000000000000000001003"),
    address!("0000000000000000000000000000000000001004"),
    address!("0000000000000000000000000000000000001005"),
    address!("0000000000000000000000000000000000001006"),
    address!("0000000000000000000000000000000000001007"),
    address!("0000000000000000000000000000000000001008"),
    address!("0000000000000000000000000000000000002000"),
    address!("b005741528b86F5952469d80A8614591E3c5B632"),
    address!("446AA6E0DC65690403dF3F127750da1322941F3e"),
];

/// Hardfork activations of BSC mainnet (chain id 56).
#[derive(Debug, Clone, Copy, Default)]
pub struct BscMainnetHardforks;

const HERTZ_BLOCK: u64 = 31_302_048;
const SHANGHAI_TIME: u64 = 1_705_996_800;
const CANCUN_TIME: u64 = 1_718_863_500;
const PRAGUE_TIME: u64 = 1_742_436_600;
const OSAKA_TIME: u64 = 1_777_343_400;

impl EthereumHardforks for BscMainnetHardforks {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        match fork {
            EthereumHardfork::Frontier
            | EthereumHardfork::Homestead
            | EthereumHardfork::Tangerine
            | EthereumHardfork::SpuriousDragon
            | EthereumHardfork::Byzantium
            | EthereumHardfork::Constantinople
            | EthereumHardfork::Petersburg
            | EthereumHardfork::Istanbul
            | EthereumHardfork::MuirGlacier => ForkCondition::Block(0),
            EthereumHardfork::Berlin | EthereumHardfork::London => {
                ForkCondition::Block(HERTZ_BLOCK)
            }
            EthereumHardfork::Shanghai => ForkCondition::Timestamp(SHANGHAI_TIME),
            EthereumHardfork::Cancun => ForkCondition::Timestamp(CANCUN_TIME),
            EthereumHardfork::Prague => ForkCondition::Timestamp(PRAGUE_TIME),
            EthereumHardfork::Osaka => ForkCondition::Timestamp(OSAKA_TIME),
            _ => ForkCondition::Never,
        }
    }
}

impl BscHardforks for BscMainnetHardforks {
    fn bsc_fork_activation(&self, fork: BscHardfork) -> ForkCondition {
        match fork {
            BscHardfork::Frontier | BscHardfork::Ramanujan | BscHardfork::Niels => {
                ForkCondition::Block(0)
            }
            BscHardfork::MirrorSync => ForkCondition::Block(5_184_000),
            BscHardfork::Bruno => ForkCondition::Block(13_082_000),
            BscHardfork::Euler => ForkCondition::Block(18_907_621),
            BscHardfork::Nano => ForkCondition::Block(21_962_149),
            BscHardfork::Moran => ForkCondition::Block(22_107_423),
            BscHardfork::Gibbs => ForkCondition::Block(23_846_001),
            BscHardfork::Planck => ForkCondition::Block(27_281_024),
            BscHardfork::Luban => ForkCondition::Block(29_020_050),
            BscHardfork::Plato => ForkCondition::Block(30_720_096),
            BscHardfork::Hertz => ForkCondition::Block(HERTZ_BLOCK),
            BscHardfork::HertzFix => ForkCondition::Block(34_140_700),
            BscHardfork::Kepler => ForkCondition::Timestamp(SHANGHAI_TIME),
            BscHardfork::Feynman | BscHardfork::FeynmanFix => {
                ForkCondition::Timestamp(1_713_419_340)
            }
            BscHardfork::Cancun | BscHardfork::Haber | BscHardfork::Tycho => {
                ForkCondition::Timestamp(CANCUN_TIME)
            }
            BscHardfork::HaberFix => ForkCondition::Timestamp(1_727_316_120),
            BscHardfork::Bohr => ForkCondition::Timestamp(1_727_317_200),
            BscHardfork::Pascal => ForkCondition::Timestamp(PRAGUE_TIME),
            BscHardfork::Lorentz => ForkCondition::Timestamp(1_745_903_100),
            BscHardfork::Maxwell => ForkCondition::Timestamp(1_751_250_600),
            BscHardfork::Fermi => ForkCondition::Timestamp(1_768_357_800),
            BscHardfork::Osaka | BscHardfork::Mendel => ForkCondition::Timestamp(OSAKA_TIME),
        }
    }
}

/// The [`BscHardfork`] active on BSC mainnet at `(block_number, timestamp)`.
pub fn bsc_mainnet_spec_at(block_number: u64, timestamp: u64) -> BscHardfork {
    let forks = BscMainnetHardforks;
    // Newest first; the first active fork wins.
    // Tycho shares Haber's activation and adds nothing the EVM sees, so
    // it is left out like in reth-bsc.
    const ORDER: &[BscHardfork] = &[
        BscHardfork::Mendel,
        BscHardfork::Osaka,
        BscHardfork::Fermi,
        BscHardfork::Maxwell,
        BscHardfork::Lorentz,
        BscHardfork::Pascal,
        BscHardfork::Bohr,
        BscHardfork::HaberFix,
        BscHardfork::Haber,
        BscHardfork::Cancun,
        BscHardfork::FeynmanFix,
        BscHardfork::Feynman,
        BscHardfork::Kepler,
        BscHardfork::HertzFix,
        BscHardfork::Hertz,
        BscHardfork::Plato,
        BscHardfork::Luban,
        BscHardfork::Planck,
        BscHardfork::Gibbs,
        BscHardfork::Moran,
        BscHardfork::Nano,
        BscHardfork::Euler,
        BscHardfork::Bruno,
        BscHardfork::MirrorSync,
        BscHardfork::Niels,
        BscHardfork::Ramanujan,
    ];
    for &fork in ORDER {
        let active = match forks.bsc_fork_activation(fork) {
            ForkCondition::Block(block) => block_number >= block,
            // Timestamp forks only exist after London on BSC.
            ForkCondition::Timestamp(time) => {
                forks.is_london_active_at_block(block_number) && timestamp >= time
            }
            _ => false,
        };
        if active {
            return fork;
        }
    }
    BscHardfork::Frontier
}

/// Whether a hardfork activates at block `block_number`, whose parent has
/// `parent_timestamp`: go-bsc upgrades system contracts only on those blocks
/// (`systemcontracts.TryUpdateBuildInSystemContract`).
pub fn bsc_mainnet_fork_transition(
    block_number: u64,
    parent_timestamp: u64,
    timestamp: u64,
) -> bool {
    let forks = BscMainnetHardforks;
    let active = |condition: ForkCondition, block_number: u64, timestamp: u64| match condition {
        ForkCondition::Block(block) => block_number >= block,
        ForkCondition::Timestamp(time) => timestamp >= time,
        _ => false,
    };
    let parent = block_number.saturating_sub(1);
    BscHardfork::VARIANTS
        .iter()
        .map(|&fork| forks.bsc_fork_activation(fork))
        .chain(
            EthereumHardfork::VARIANTS
                .iter()
                .map(|&fork| forks.ethereum_fork_activation(fork)),
        )
        .any(|condition| {
            active(condition, block_number, timestamp)
                != active(condition, parent, parent_timestamp)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_transitions() {
        assert!(bsc_mainnet_fork_transition(HERTZ_BLOCK, 0, 0));
        assert!(!bsc_mainnet_fork_transition(HERTZ_BLOCK + 1, 0, 0));
        assert!(bsc_mainnet_fork_transition(34_140_700, 0, 0));
        let at = 40_000_000;
        assert!(bsc_mainnet_fork_transition(
            at,
            CANCUN_TIME - 3,
            CANCUN_TIME
        ));
        assert!(!bsc_mainnet_fork_transition(
            at,
            CANCUN_TIME,
            CANCUN_TIME + 3
        ));
        assert!(!bsc_mainnet_fork_transition(
            at,
            CANCUN_TIME - 6,
            CANCUN_TIME - 3
        ));
    }

    #[test]
    fn spec_follows_mainnet_schedule() {
        assert_eq!(bsc_mainnet_spec_at(0, 0), BscHardfork::Niels);
        assert_eq!(bsc_mainnet_spec_at(22_107_423, 0), BscHardfork::Moran);
        assert_eq!(bsc_mainnet_spec_at(31_302_047, 0), BscHardfork::Plato);
        assert_eq!(bsc_mainnet_spec_at(31_302_048, 0), BscHardfork::Hertz);
        assert_eq!(
            bsc_mainnet_spec_at(40_000_000, SHANGHAI_TIME),
            BscHardfork::Kepler
        );
        assert_eq!(
            bsc_mainnet_spec_at(40_000_000, CANCUN_TIME),
            BscHardfork::Haber
        );
        assert_eq!(
            bsc_mainnet_spec_at(40_000_000, PRAGUE_TIME),
            BscHardfork::Pascal
        );
        assert_eq!(
            bsc_mainnet_spec_at(80_000_000, OSAKA_TIME),
            BscHardfork::Mendel
        );
    }
}
