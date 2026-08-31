use leafage_evm_types::{Header, MainnetSpecId};

use super::{ArcForkActivation, ArcHardforkFlags, ArcHardforkSchedule};

pub const ARC_MAINNET_CHAIN_ID: u64 = 5042;

const ARC_BASE_FEE_FIXED_POINT_SCALE: u128 = 10_000;

/// Static Arc fee parameters used only when a parent header predates the
/// eight-byte `nextBaseFee` extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArcBaseFeeConfig {
    pub k_rate: u64,
    pub inverse_elasticity_multiplier: u64,
    pub min_base_fee: u64,
    pub max_base_fee: u64,
}

impl ArcBaseFeeConfig {
    pub const fn mainnet() -> Self {
        Self {
            k_rate: 200,
            inverse_elasticity_multiplier: 5_000,
            min_base_fee: 1,
            max_base_fee: 20_000_000_000_000,
        }
    }

    pub fn next_block_base_fee(&self, parent: &Header) -> u64 {
        let base_fee = parent.base_fee_per_gas.unwrap_or_default();
        let gas_target = (parent.gas_limit as u128)
            .saturating_mul(self.inverse_elasticity_multiplier as u128)
            / ARC_BASE_FEE_FIXED_POINT_SCALE;
        if gas_target == 0 || self.k_rate == 0 {
            return base_fee.clamp(self.min_base_fee, self.max_base_fee);
        }

        let denominator =
            gas_target.saturating_mul(ARC_BASE_FEE_FIXED_POINT_SCALE) / self.k_rate as u128;
        if denominator == 0 {
            return base_fee.clamp(self.min_base_fee, self.max_base_fee);
        }

        let gas_used = parent.gas_used as u128;
        let base_fee_u128 = base_fee as u128;
        let next = match gas_used.cmp(&gas_target) {
            std::cmp::Ordering::Equal => base_fee,
            std::cmp::Ordering::Greater => {
                let increase = base_fee_u128.saturating_mul(gas_used - gas_target) / denominator;
                base_fee.saturating_add(u64::try_from(increase).unwrap_or(u64::MAX).max(1))
            }
            std::cmp::Ordering::Less => {
                let decrease = base_fee_u128.saturating_mul(gas_target - gas_used) / denominator;
                base_fee.saturating_sub(u64::try_from(decrease).unwrap_or(u64::MAX))
            }
        };
        next.clamp(self.min_base_fee, self.max_base_fee)
    }
}

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
    base_fee: ArcBaseFeeConfig,
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
            base_fee: ArcBaseFeeConfig::mainnet(),
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

    pub const fn base_fee_config(&self) -> ArcBaseFeeConfig {
        self.base_fee
    }

    pub fn fallback_next_base_fee(&self, parent: &Header) -> u64 {
        self.base_fee.next_block_base_fee(parent)
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

    #[test]
    fn mainnet_static_base_fee_fallback_matches_arc_defaults_and_clamps() {
        let config = ArcChainConfig::mainnet();
        assert_eq!(
            config.base_fee_config(),
            ArcBaseFeeConfig {
                k_rate: 200,
                inverse_elasticity_multiplier: 5_000,
                min_base_fee: 1,
                max_base_fee: 20_000_000_000_000,
            }
        );

        let mut parent: Header = Header::default();
        parent.inner.gas_limit = 30_000_000;
        parent.inner.gas_used = 0;
        parent.inner.base_fee_per_gas = Some(1_000_000_000);
        assert_eq!(config.fallback_next_base_fee(&parent), 980_000_000);

        parent.inner.base_fee_per_gas = Some(1);
        assert_eq!(config.fallback_next_base_fee(&parent), 1);
        parent.inner.base_fee_per_gas = Some(u64::MAX);
        assert_eq!(
            config.fallback_next_base_fee(&parent),
            config.base_fee_config().max_base_fee
        );
    }
}
