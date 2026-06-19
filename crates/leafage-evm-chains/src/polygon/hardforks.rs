use crate::polygon::gas::apply_gas_rules;
use alloy_hardforks::{hardfork, ForkCondition};
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::primitives::hardfork::SpecId;

const ISTANBUL_ACTIVATION_BLOCK: u64 = 3_395_000;
const BERLIN_ACTIVATION_BLOCK: u64 = 14_750_000;
const LONDON_ACTIVATION_BLOCK: u64 = 23_850_000;
const SHANGHAI_ACTIVATION_BLOCK: u64 = 50_523_000;
const CANCUN_ACTIVATION_BLOCK: u64 = 54_876_000;
const PRAGUE_ACTIVATION_BLOCK: u64 = 73_440_256;
const MADHUGIRI_ACTIVATION_BLOCK: u64 = 80_084_800;
const LISOVO_ACTIVATION_BLOCK: u64 = 83_756_500;
const CHICAGO_ACTIVATION_BLOCK: u64 = 87_218_600;

hardfork!(
    /// The name of a Polygon hardfork.
    #[derive(Default)]
    PolygonHardfork {
        /// Initial supported Polygon hardfork.
        Petersburg,
        /// Polygon Istanbul hardfork.
        Istanbul,
        /// Polygon Berlin hardfork.
        Berlin,
        /// Polygon London hardfork.
        London,
        /// Polygon Shanghai hardfork.
        Shanghai,
        /// Polygon Cancun hardfork.
        Cancun,
        /// Polygon Prague hardfork.
        Prague,
        /// Polygon Madhugiri hardfork.
        Madhugiri,
        /// Polygon Lisovo hardfork.
        Lisovo,
        /// Polygon Chicago hardfork.
        #[default]
        Chicago,
    }
);

impl PolygonHardfork {
    pub fn from_block_number(block_number: u64) -> Self {
        Self::VARIANTS
            .iter()
            .rev()
            .copied()
            .find(|fork| fork.fork_activation().active_at_block(block_number))
            .unwrap_or(Self::Petersburg)
    }

    pub const fn fork_activation(self) -> ForkCondition {
        match self {
            Self::Petersburg => ForkCondition::ZERO_BLOCK,
            Self::Istanbul => ForkCondition::Block(ISTANBUL_ACTIVATION_BLOCK),
            Self::Berlin => ForkCondition::Block(BERLIN_ACTIVATION_BLOCK),
            Self::London => ForkCondition::Block(LONDON_ACTIVATION_BLOCK),
            Self::Shanghai => ForkCondition::Block(SHANGHAI_ACTIVATION_BLOCK),
            Self::Cancun => ForkCondition::Block(CANCUN_ACTIVATION_BLOCK),
            Self::Prague => ForkCondition::Block(PRAGUE_ACTIVATION_BLOCK),
            Self::Madhugiri => ForkCondition::Block(MADHUGIRI_ACTIVATION_BLOCK),
            Self::Lisovo => ForkCondition::Block(LISOVO_ACTIVATION_BLOCK),
            Self::Chicago => ForkCondition::Block(CHICAGO_ACTIVATION_BLOCK),
        }
    }

    pub fn apply_to_cfg(self, base_cfg: &CfgEnv<PolygonHardfork>) -> CfgEnv<PolygonHardfork> {
        let mut cfg = base_cfg.clone();
        cfg.set_spec_and_mainnet_gas_params(self);
        apply_gas_rules(self, &mut cfg);
        cfg
    }

    pub fn is_pip88_enabled(self) -> bool {
        self >= Self::Chicago
    }

    pub(crate) fn is_madhugiri_enabled(self) -> bool {
        self >= Self::Madhugiri
    }
}

impl From<PolygonHardfork> for CfgEnv<PolygonHardfork> {
    fn from(spec: PolygonHardfork) -> Self {
        spec.apply_to_cfg(&CfgEnv::default())
    }
}

impl From<&BlockEnv> for PolygonHardfork {
    fn from(block_env: &BlockEnv) -> Self {
        let block_number: u64 = block_env.number.saturating_to();
        Self::from_block_number(block_number)
    }
}

impl TryFrom<u8> for PolygonHardfork {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::VARIANTS.get(usize::from(value)).copied().ok_or(())
    }
}

impl From<PolygonHardfork> for SpecId {
    fn from(spec: PolygonHardfork) -> Self {
        match spec {
            PolygonHardfork::Petersburg => SpecId::PETERSBURG,
            PolygonHardfork::Istanbul => SpecId::ISTANBUL,
            PolygonHardfork::Berlin => SpecId::BERLIN,
            PolygonHardfork::London => SpecId::LONDON,
            PolygonHardfork::Shanghai => SpecId::SHANGHAI,
            PolygonHardfork::Cancun => SpecId::CANCUN,
            PolygonHardfork::Prague
            | PolygonHardfork::Madhugiri
            | PolygonHardfork::Lisovo
            | PolygonHardfork::Chicago => SpecId::PRAGUE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::polygon::gas::{
        COLD_SSTORE_ADDITIONAL_COST_PIP88, COLD_SSTORE_COST_PIP88,
        SSTORE_CLEARS_SCHEDULE_REFUND_PIP88, SSTORE_RESET_WITHOUT_COLD_LOAD_COST_PIP88,
        TX_GAS_LIMIT_CAP_POST_MADHUGIRI,
    };
    use revm::primitives::U256;

    fn block_env(number: u64) -> BlockEnv {
        let mut block_env = BlockEnv::default();
        block_env.number = U256::from(number);
        block_env
    }

    #[test]
    fn resolves_chicago_at_pip88_block() {
        assert_eq!(
            PolygonHardfork::from_block_number(CHICAGO_ACTIVATION_BLOCK - 1),
            PolygonHardfork::Lisovo
        );
        assert_eq!(
            PolygonHardfork::from_block_number(CHICAGO_ACTIVATION_BLOCK),
            PolygonHardfork::Chicago
        );
    }

    #[test]
    fn resolves_hardfork_from_activation_conditions() {
        for (fork, block_number) in [
            (PolygonHardfork::Petersburg, 0),
            (PolygonHardfork::Istanbul, ISTANBUL_ACTIVATION_BLOCK),
            (PolygonHardfork::Berlin, BERLIN_ACTIVATION_BLOCK),
            (PolygonHardfork::London, LONDON_ACTIVATION_BLOCK),
            (PolygonHardfork::Shanghai, SHANGHAI_ACTIVATION_BLOCK),
            (PolygonHardfork::Cancun, CANCUN_ACTIVATION_BLOCK),
            (PolygonHardfork::Prague, PRAGUE_ACTIVATION_BLOCK),
            (PolygonHardfork::Madhugiri, MADHUGIRI_ACTIVATION_BLOCK),
            (PolygonHardfork::Lisovo, LISOVO_ACTIVATION_BLOCK),
            (PolygonHardfork::Chicago, CHICAGO_ACTIVATION_BLOCK),
        ] {
            assert!(fork.fork_activation().active_at_block(block_number));
            assert_eq!(PolygonHardfork::from_block_number(block_number), fork);
        }
    }

    #[test]
    fn resolves_cli_spec_id_from_variant_order() {
        assert_eq!(PolygonHardfork::try_from(9), Ok(PolygonHardfork::Chicago));
        assert_eq!(PolygonHardfork::try_from(10), Err(()));
    }

    #[test]
    fn resolves_hardfork_from_block_env() {
        assert_eq!(
            PolygonHardfork::from(&block_env(CHICAGO_ACTIVATION_BLOCK)),
            PolygonHardfork::Chicago
        );
    }

    #[test]
    fn applies_madhugiri_tx_gas_cap() {
        let mut cfg = CfgEnv::new_with_spec(PolygonHardfork::Chicago);
        cfg.tx_gas_limit_cap = Some(u64::MAX);
        let block_env = block_env(MADHUGIRI_ACTIVATION_BLOCK);

        let cfg = PolygonHardfork::from(&block_env).apply_to_cfg(&cfg);

        assert_eq!(cfg.tx_gas_limit_cap, Some(TX_GAS_LIMIT_CAP_POST_MADHUGIRI));
    }

    #[test]
    fn pip88_uses_sstore_cold_cost_in_cfg_table() {
        let base_cfg = CfgEnv::default();
        let block_env = block_env(CHICAGO_ACTIVATION_BLOCK);
        let cfg = PolygonHardfork::from(&block_env).apply_to_cfg(&base_cfg);

        assert_eq!(
            cfg.gas_params.cold_storage_additional_cost(),
            COLD_SSTORE_ADDITIONAL_COST_PIP88
        );
        assert_eq!(cfg.gas_params.cold_storage_cost(), COLD_SSTORE_COST_PIP88);
        assert_eq!(
            cfg.gas_params.sstore_clearing_slot_refund(),
            SSTORE_CLEARS_SCHEDULE_REFUND_PIP88
        );
        assert_eq!(
            cfg.gas_params.sstore_reset_without_cold_load_cost(),
            SSTORE_RESET_WITHOUT_COLD_LOAD_COST_PIP88
        );
        assert_eq!(
            cfg.gas_params.sstore_reset_refund(),
            SSTORE_RESET_WITHOUT_COLD_LOAD_COST_PIP88
        );
    }
}
