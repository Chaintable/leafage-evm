use leafage_evm_types::MainnetSpecId;

use super::{ArcForkActivation, ArcHardforkFlags, ArcHardforkSchedule};

pub const ARC_MAINNET_CHAIN_ID: u64 = 5042;

/// Ethereum and Arc hardfork state for one execution environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArcExecutionSpec {
    pub ethereum_spec: MainnetSpecId,
    pub arc_flags: ArcHardforkFlags,
}

/// Arc chain configuration kept separate from Ethereum mainnet configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArcChainConfig {
    chain_id: u64,
    ethereum_spec: MainnetSpecId,
    hardforks: ArcHardforkSchedule,
}

impl ArcChainConfig {
    pub const fn mainnet() -> Self {
        Self {
            chain_id: ARC_MAINNET_CHAIN_ID,
            ethereum_spec: MainnetSpecId::OSAKA,
            hardforks: ArcHardforkSchedule::new(
                ArcForkActivation::Block(0),
                ArcForkActivation::Block(0),
                ArcForkActivation::Block(0),
                ArcForkActivation::Block(0),
                ArcForkActivation::Never,
                ArcForkActivation::Never,
            ),
        }
    }

    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub const fn ethereum_spec(&self) -> MainnetSpecId {
        self.ethereum_spec
    }

    pub const fn hardforks(&self) -> &ArcHardforkSchedule {
        &self.hardforks
    }

    pub fn execution_spec_at(&self, block_number: u64, timestamp: u64) -> ArcExecutionSpec {
        ArcExecutionSpec {
            ethereum_spec: self.ethereum_spec,
            arc_flags: self.hardforks.flags_at(block_number, timestamp),
        }
    }
}

impl Default for ArcChainConfig {
    fn default() -> Self {
        Self::mainnet()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arc::ArcHardfork;

    #[test]
    fn mainnet_uses_arc_chain_id_and_osaka() {
        let config = ArcChainConfig::mainnet();

        assert_eq!(config.chain_id(), ARC_MAINNET_CHAIN_ID);
        assert_eq!(config.ethereum_spec(), MainnetSpecId::OSAKA);
    }

    #[test]
    fn mainnet_arc_forks_match_v0_7_3_schedule() {
        let config = ArcChainConfig::mainnet();

        for (hardfork, activation) in [
            (ArcHardfork::Zero3, ArcForkActivation::Block(0)),
            (ArcHardfork::Zero4, ArcForkActivation::Block(0)),
            (ArcHardfork::Zero5, ArcForkActivation::Block(0)),
            (ArcHardfork::Zero6, ArcForkActivation::Block(0)),
            (ArcHardfork::Zero7, ArcForkActivation::Never),
            (ArcHardfork::Zero8, ArcForkActivation::Never),
        ] {
            assert_eq!(config.hardforks().activation(hardfork), activation);
        }

        let genesis = config.execution_spec_at(0, 0);
        for hardfork in [
            ArcHardfork::Zero3,
            ArcHardfork::Zero4,
            ArcHardfork::Zero5,
            ArcHardfork::Zero6,
        ] {
            assert!(genesis.arc_flags.is_active(hardfork));
        }
        assert!(!genesis.arc_flags.is_active(ArcHardfork::Zero7));
        assert!(!genesis.arc_flags.is_active(ArcHardfork::Zero8));

        let latest = config.execution_spec_at(u64::MAX, u64::MAX);
        assert!(!latest.arc_flags.is_active(ArcHardfork::Zero7));
        assert!(!latest.arc_flags.is_active(ArcHardfork::Zero8));
    }

    #[test]
    fn execution_spec_keeps_ethereum_and_arc_hardfork_state_separate() {
        let spec = ArcChainConfig::mainnet().execution_spec_at(0, 0);

        assert_eq!(spec.ethereum_spec, MainnetSpecId::OSAKA);
        assert!(spec.arc_flags.is_active(ArcHardfork::Zero3));
    }
}
