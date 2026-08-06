use alloy_hardforks::hardfork;
use leafage_evm_types::MainnetSpecId;
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
}
