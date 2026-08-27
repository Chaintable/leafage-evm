use alloy_hardforks::hardfork;
use leafage_evm_types::{CfgEnv, MainnetSpecId};
use revm::primitives::hardfork::SpecId;

hardfork!(
    /// The name of an Arbitrum EVM hardfork.
    ///
    /// ArbOS feature gates are tracked separately from this enum because Nitro
    /// enables some ArbOS precompiles by ArbOS version rather than by Ethereum
    /// execution spec.
    #[derive(Default)]
    ArbitrumHardfork {
        /// Initial Nitro-era EVM baseline used by older ArbOS versions.
        Berlin,
        /// Ethereum London execution rules.
        London,
        /// Ethereum Shanghai execution rules.
        Shanghai,
        /// Ethereum Cancun execution rules.
        Cancun,
        /// Ethereum Prague execution rules.
        Prague,
        /// Ethereum Osaka execution rules.
        #[default]
        Osaka,
        /// Ethereum Amsterdam execution rules.
        Amsterdam,
    }
);

impl From<ArbitrumHardfork> for SpecId {
    fn from(spec: ArbitrumHardfork) -> Self {
        match spec {
            ArbitrumHardfork::Berlin => SpecId::BERLIN,
            ArbitrumHardfork::London => SpecId::LONDON,
            ArbitrumHardfork::Shanghai => SpecId::SHANGHAI,
            ArbitrumHardfork::Cancun => SpecId::CANCUN,
            ArbitrumHardfork::Prague => SpecId::PRAGUE,
            ArbitrumHardfork::Osaka => SpecId::OSAKA,
            ArbitrumHardfork::Amsterdam => SpecId::AMSTERDAM,
        }
    }
}

impl ArbitrumHardfork {
    /// Returns the Ethereum execution spec Nitro activates for an ArbOS version.
    pub const fn from_arbos_version(version: u64) -> Self {
        match version {
            50.. => Self::Osaka,
            40..=49 => Self::Prague,
            20..=39 => Self::Cancun,
            11..=19 => Self::Shanghai,
            _ => Self::London,
        }
    }

    /// Updates the active EVM spec and all fork-dependent gas parameters.
    pub fn apply_cfg(self, cfg: &mut CfgEnv<Self>) {
        cfg.set_spec_and_mainnet_gas_params(self);
    }
}

impl From<MainnetSpecId> for ArbitrumHardfork {
    fn from(spec: MainnetSpecId) -> Self {
        match spec {
            MainnetSpecId::FRONTIER
            | MainnetSpecId::FRONTIER_THAWING
            | MainnetSpecId::HOMESTEAD
            | MainnetSpecId::DAO_FORK
            | MainnetSpecId::TANGERINE
            | MainnetSpecId::SPURIOUS_DRAGON
            | MainnetSpecId::BYZANTIUM
            | MainnetSpecId::CONSTANTINOPLE
            | MainnetSpecId::PETERSBURG
            | MainnetSpecId::ISTANBUL
            | MainnetSpecId::MUIR_GLACIER
            | MainnetSpecId::BERLIN => Self::Berlin,
            MainnetSpecId::LONDON
            | MainnetSpecId::ARROW_GLACIER
            | MainnetSpecId::GRAY_GLACIER
            | MainnetSpecId::MERGE => Self::London,
            MainnetSpecId::SHANGHAI => Self::Shanghai,
            MainnetSpecId::CANCUN => Self::Cancun,
            MainnetSpecId::PRAGUE => Self::Prague,
            MainnetSpecId::OSAKA => Self::Osaka,
            MainnetSpecId::AMSTERDAM => Self::Amsterdam,
        }
    }
}

impl TryFrom<u8> for ArbitrumHardfork {
    type Error = <MainnetSpecId as TryFrom<u8>>::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        // CLI compatibility: `--spec-id` uses mainnet SpecId discriminants, not
        // ArbitrumHardfork variant indexes.
        MainnetSpecId::try_from(value).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_arbitrum_hardfork_to_revm_spec() {
        for (hardfork, spec) in [
            (ArbitrumHardfork::Berlin, SpecId::BERLIN),
            (ArbitrumHardfork::London, SpecId::LONDON),
            (ArbitrumHardfork::Shanghai, SpecId::SHANGHAI),
            (ArbitrumHardfork::Cancun, SpecId::CANCUN),
            (ArbitrumHardfork::Prague, SpecId::PRAGUE),
            (ArbitrumHardfork::Osaka, SpecId::OSAKA),
            (ArbitrumHardfork::Amsterdam, SpecId::AMSTERDAM),
        ] {
            assert_eq!(SpecId::from(hardfork), spec);
        }
    }

    #[test]
    fn accepts_mainnet_spec_as_compatibility_input() {
        assert_eq!(
            ArbitrumHardfork::from(MainnetSpecId::BERLIN),
            ArbitrumHardfork::Berlin
        );
        assert_eq!(
            ArbitrumHardfork::from(MainnetSpecId::MERGE),
            ArbitrumHardfork::London
        );
        assert_eq!(
            ArbitrumHardfork::from(MainnetSpecId::PRAGUE),
            ArbitrumHardfork::Prague
        );
        assert_eq!(
            ArbitrumHardfork::from(MainnetSpecId::AMSTERDAM),
            ArbitrumHardfork::Amsterdam
        );
    }

    #[test]
    fn accepts_cli_spec_ids_as_mainnet_compatibility_input() {
        assert_eq!(
            ArbitrumHardfork::try_from(MainnetSpecId::FRONTIER_THAWING as u8),
            Ok(ArbitrumHardfork::Berlin)
        );
        assert_eq!(
            ArbitrumHardfork::try_from(MainnetSpecId::PRAGUE as u8),
            Ok(ArbitrumHardfork::Prague)
        );
        assert_eq!(
            ArbitrumHardfork::try_from(MainnetSpecId::AMSTERDAM as u8),
            Ok(ArbitrumHardfork::Amsterdam)
        );
    }

    #[test]
    fn maps_arbos_versions_to_nitro_evm_hardforks() {
        for (version, expected) in [
            (0, ArbitrumHardfork::London),
            (10, ArbitrumHardfork::London),
            (11, ArbitrumHardfork::Shanghai),
            (19, ArbitrumHardfork::Shanghai),
            (20, ArbitrumHardfork::Cancun),
            (39, ArbitrumHardfork::Cancun),
            (40, ArbitrumHardfork::Prague),
            (49, ArbitrumHardfork::Prague),
            (50, ArbitrumHardfork::Osaka),
            (61, ArbitrumHardfork::Osaka),
        ] {
            assert_eq!(ArbitrumHardfork::from_arbos_version(version), expected);
        }
    }
}
