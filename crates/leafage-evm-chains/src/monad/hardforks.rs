//! Monad revision table (`execution/monad/chain/monad_mainnet.cpp`) and the
//! feature flags derived from it (`vm/evm/traits.hpp` `MonadTraits`).

use crate::monad::gas::apply_gas_rules;
use alloy_hardforks::{hardfork, ForkCondition};
use leafage_evm_types::CfgEnv;
use revm::primitives::hardfork::SpecId;
use revm::primitives::U256;

const MONAD_THREE_ACTIVATION_TIMESTAMP: u64 = 1_755_091_800; // 2025-08-13T13:30:00Z
const MONAD_SIX_ACTIVATION_TIMESTAMP: u64 = 1_762_266_600; // 2025-11-04T14:30:00Z
const MONAD_SEVEN_ACTIVATION_TIMESTAMP: u64 = 1_762_525_800; // 2025-11-07T14:30:00Z
const MONAD_EIGHT_ACTIVATION_TIMESTAMP: u64 = 1_763_649_000; // 2025-11-20T14:30:00Z
const MONAD_NINE_ACTIVATION_TIMESTAMP: u64 = 1_773_930_600; // 2026-03-19T14:30:00Z
const MONAD_TEN_ACTIVATION_TIMESTAMP: u64 = 1_788_359_400; // 2026-09-02T14:30:00Z

/// `constants::MAX_CODE_SIZE_EIP170`
const MAX_CODE_SIZE_EIP170: usize = 24 * 1024;
/// `constants::MAX_CODE_SIZE_MONAD_TWO`
const MAX_CODE_SIZE_MONAD_TWO: usize = 128 * 1024;

/// 1 MON in wei.
pub(crate) const MON: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

hardfork!(
    /// Monad revision (`monad_revision` in `vm/evm/monad/revision.h`).
    ///
    /// Only revisions that exist on mainnet are listed. `MONAD_FOUR` and
    /// `MONAD_FIVE` never activated on mainnet (the chain jumped from THREE to
    /// SIX), they are kept so the `>=` feature gates read like the C++ code.
    #[derive(Default)]
    MonadHardfork {
        /// Pre-mainnet revision. Cancun.
        MonadTwo,
        /// Cancun.
        MonadThree,
        /// Prague, staking precompile, P256 precompile. Not activated on mainnet.
        MonadFour,
        /// `getProposerValId`, lower active validator stake. Not activated on mainnet.
        MonadFive,
        /// First mainnet revision.
        MonadSix,
        /// Pricing version 1.
        MonadSeven,
        /// Linked list pagination 50.
        MonadEight,
        /// Osaka, MIP-3 memory, reserve balance precompile.
        MonadNine,
        /// MIP-8 page pricing.
        #[default]
        MonadTen,
    }
);

impl MonadHardfork {
    /// `MonadMainnet::get_monad_revision`.
    pub fn active_at_timestamp(timestamp: u64) -> Self {
        Self::VARIANTS
            .iter()
            .rev()
            .copied()
            .find(|fork| fork.fork_activation().active_at_timestamp(timestamp))
            .unwrap_or(Self::MonadTwo)
    }

    pub const fn fork_activation(self) -> ForkCondition {
        match self {
            Self::MonadTwo => ForkCondition::Timestamp(0),
            Self::MonadThree => ForkCondition::Timestamp(MONAD_THREE_ACTIVATION_TIMESTAMP),
            Self::MonadFour | Self::MonadFive => ForkCondition::Never,
            Self::MonadSix => ForkCondition::Timestamp(MONAD_SIX_ACTIVATION_TIMESTAMP),
            Self::MonadSeven => ForkCondition::Timestamp(MONAD_SEVEN_ACTIVATION_TIMESTAMP),
            Self::MonadEight => ForkCondition::Timestamp(MONAD_EIGHT_ACTIVATION_TIMESTAMP),
            Self::MonadNine => ForkCondition::Timestamp(MONAD_NINE_ACTIVATION_TIMESTAMP),
            Self::MonadTen => ForkCondition::Timestamp(MONAD_TEN_ACTIVATION_TIMESTAMP),
        }
    }

    pub fn apply_cfg(self, cfg: &mut CfgEnv<MonadHardfork>) {
        cfg.set_spec_and_mainnet_gas_params(self);
        cfg.limit_contract_code_size = Some(self.max_code_size());
        cfg.limit_contract_initcode_size = Some(self.max_initcode_size());
        apply_gas_rules(self, cfg);
    }

    /// `MonadTraits::evm_rev`.
    pub const fn evm_spec(self) -> SpecId {
        if self.is_at_least(Self::MonadNine) {
            SpecId::OSAKA
        } else if self.is_at_least(Self::MonadFour) {
            SpecId::PRAGUE
        } else {
            SpecId::CANCUN
        }
    }

    const fn is_at_least(self, other: Self) -> bool {
        self as u8 >= other as u8
    }

    /// `MonadTraits::monad_pricing_version() >= 1`: cold access repricing and
    /// precompile gas multipliers.
    pub const fn is_pricing_v1_enabled(self) -> bool {
        self.is_at_least(Self::MonadSeven)
    }

    /// `MonadTraits::mip_3_active`: linear memory pricing + 8 MB cap.
    pub const fn is_mip3_enabled(self) -> bool {
        self.is_at_least(Self::MonadNine)
    }

    /// `MonadTraits::mip_8_active`: page based storage pricing.
    pub const fn is_mip8_enabled(self) -> bool {
        self.is_at_least(Self::MonadTen)
    }

    /// Staking precompile (`0x1000`) is callable.
    pub const fn is_staking_enabled(self) -> bool {
        self.is_at_least(Self::MonadFour)
    }

    /// `getProposerValId()` dispatches (before MONAD_FIVE it hits the fallback).
    pub const fn is_proposer_val_id_enabled(self) -> bool {
        self.is_at_least(Self::MonadFive)
    }

    /// Reserve balance precompile (`0x1001`) exists.
    pub const fn is_reserve_balance_enabled(self) -> bool {
        self.is_at_least(Self::MonadNine)
    }

    /// `MonadTraits::eip_7951_active`.
    pub const fn is_p256_enabled(self) -> bool {
        self.is_at_least(Self::MonadFour)
    }

    /// Prague semantics: CREATE is rejected inside a delegated (EIP-7702)
    /// account because `can_create_inside_delegated() == false`.
    pub const fn is_delegated_create_blocked(self) -> bool {
        self.is_at_least(Self::MonadFour)
    }

    /// `MonadTraits::max_code_size`.
    pub const fn max_code_size(self) -> usize {
        if self.is_at_least(Self::MonadTwo) {
            MAX_CODE_SIZE_MONAD_TWO
        } else {
            MAX_CODE_SIZE_EIP170
        }
    }

    /// `MonadTraits::max_initcode_size`.
    pub const fn max_initcode_size(self) -> usize {
        if self.is_at_least(Self::MonadFour) {
            2 * MAX_CODE_SIZE_MONAD_TWO
        } else {
            2 * MAX_CODE_SIZE_EIP170
        }
    }

    /// `limits::linked_list_pagination`.
    pub const fn linked_list_pagination(self) -> u32 {
        if self.is_at_least(Self::MonadEight) {
            50
        } else {
            100
        }
    }

    /// `limits::active_validator_stake`.
    pub fn active_validator_stake(self) -> U256 {
        let mon = if self.is_at_least(Self::MonadFive) {
            10_000_000u64
        } else {
            25_000_000u64
        };
        MON * U256::from(mon)
    }
}

impl TryFrom<u8> for MonadHardfork {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::VARIANTS.get(usize::from(value)).copied().ok_or(())
    }
}

impl From<MonadHardfork> for SpecId {
    fn from(spec: MonadHardfork) -> Self {
        spec.evm_spec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_hardfork_from_timestamp() {
        for (fork, ts) in [
            (MonadHardfork::MonadTwo, 0),
            (
                MonadHardfork::MonadTwo,
                MONAD_THREE_ACTIVATION_TIMESTAMP - 1,
            ),
            (MonadHardfork::MonadThree, MONAD_THREE_ACTIVATION_TIMESTAMP),
            (
                MonadHardfork::MonadThree,
                MONAD_SIX_ACTIVATION_TIMESTAMP - 1,
            ),
            (MonadHardfork::MonadSix, MONAD_SIX_ACTIVATION_TIMESTAMP),
            (MonadHardfork::MonadSeven, MONAD_SEVEN_ACTIVATION_TIMESTAMP),
            (MonadHardfork::MonadEight, MONAD_EIGHT_ACTIVATION_TIMESTAMP),
            (MonadHardfork::MonadNine, MONAD_NINE_ACTIVATION_TIMESTAMP),
            (MonadHardfork::MonadTen, MONAD_TEN_ACTIVATION_TIMESTAMP),
            (MonadHardfork::MonadTen, u64::MAX),
        ] {
            assert_eq!(MonadHardfork::active_at_timestamp(ts), fork, "ts {ts}");
        }
    }

    #[test]
    fn four_and_five_never_activate_on_mainnet() {
        for ts in [0, MONAD_SIX_ACTIVATION_TIMESTAMP, u64::MAX] {
            let fork = MonadHardfork::active_at_timestamp(ts);
            assert!(!matches!(
                fork,
                MonadHardfork::MonadFour | MonadHardfork::MonadFive
            ));
        }
    }

    #[test]
    fn evm_spec_matches_monad_traits() {
        assert_eq!(MonadHardfork::MonadTwo.evm_spec(), SpecId::CANCUN);
        assert_eq!(MonadHardfork::MonadThree.evm_spec(), SpecId::CANCUN);
        assert_eq!(MonadHardfork::MonadFour.evm_spec(), SpecId::PRAGUE);
        assert_eq!(MonadHardfork::MonadSix.evm_spec(), SpecId::PRAGUE);
        assert_eq!(MonadHardfork::MonadEight.evm_spec(), SpecId::PRAGUE);
        assert_eq!(MonadHardfork::MonadNine.evm_spec(), SpecId::OSAKA);
        assert_eq!(MonadHardfork::MonadTen.evm_spec(), SpecId::OSAKA);
    }

    #[test]
    fn feature_gates_follow_revision_table() {
        assert!(!MonadHardfork::MonadSix.is_pricing_v1_enabled());
        assert!(MonadHardfork::MonadSeven.is_pricing_v1_enabled());
        assert!(!MonadHardfork::MonadEight.is_mip3_enabled());
        assert!(MonadHardfork::MonadNine.is_mip3_enabled());
        assert!(!MonadHardfork::MonadNine.is_mip8_enabled());
        assert!(MonadHardfork::MonadTen.is_mip8_enabled());
        assert!(!MonadHardfork::MonadThree.is_staking_enabled());
        assert!(MonadHardfork::MonadSix.is_staking_enabled());
        assert!(!MonadHardfork::MonadEight.is_reserve_balance_enabled());
        assert!(MonadHardfork::MonadNine.is_reserve_balance_enabled());
        assert_eq!(MonadHardfork::MonadSeven.linked_list_pagination(), 100);
        assert_eq!(MonadHardfork::MonadEight.linked_list_pagination(), 50);
        assert_eq!(
            MonadHardfork::MonadSix.active_validator_stake(),
            MON * U256::from(10_000_000u64)
        );
    }

    #[test]
    fn cfg_limits_match_monad_constants() {
        let mut cfg = CfgEnv::new_with_spec(MonadHardfork::MonadTen);
        MonadHardfork::MonadTen.apply_cfg(&mut cfg);
        assert_eq!(cfg.limit_contract_code_size, Some(128 * 1024));
        assert_eq!(cfg.limit_contract_initcode_size, Some(256 * 1024));
        assert_eq!(cfg.spec, MonadHardfork::MonadTen);

        let mut cfg = CfgEnv::new_with_spec(MonadHardfork::MonadThree);
        MonadHardfork::MonadThree.apply_cfg(&mut cfg);
        assert_eq!(cfg.limit_contract_code_size, Some(128 * 1024));
        assert_eq!(cfg.limit_contract_initcode_size, Some(48 * 1024));
    }

    #[test]
    fn resolves_cli_spec_id_from_variant_order() {
        assert_eq!(MonadHardfork::try_from(8), Ok(MonadHardfork::MonadTen));
        assert_eq!(MonadHardfork::try_from(9), Err(()));
    }
}
