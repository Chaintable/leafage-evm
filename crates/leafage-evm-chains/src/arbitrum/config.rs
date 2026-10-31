use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// Execution engine selection. Classic and Nitro share a chain ID, so this
/// must be explicit rather than inferred from chain ID or an empty header.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArbitrumExecutionMode {
    #[default]
    Nitro,
    Classic,
}

/// Historical Classic MODEXP upgrades, expressed in L2 block numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct ClassicModexpUpgrades {
    pub exponent_size_block: u64,
    pub uint_fast_path_block: u64,
}

/// Classic builtin validation upgrades, expressed in L2 block numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct ClassicBuiltinUpgrades {
    pub arbos_34_block: u64,
    pub arbos_39_block: u64,
    pub arbos_42_block: u64,
    pub arbos_54_block: u64,
}

impl ClassicBuiltinUpgrades {
    pub fn for_chain_id(chain_id: u64) -> Option<Self> {
        (chain_id == 42161).then_some(Self {
            // Boundaries verified against adjacent native archive calls.
            arbos_34_block: 156_077,
            arbos_39_block: 226_011,
            arbos_42_block: 662_405,
            arbos_54_block: 6_835_288,
        })
    }
}

/// Per-chain configuration for Arbitrum replicas, parsed from
/// `--evm-custom-config`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ArbitrumEvmConfig {
    /// Classic enables historical call simulation from account StateDiffs.
    /// It does not provide AVM gas accounting or missing private ArbOS state.
    #[serde(default)]
    pub execution_mode: ArbitrumExecutionMode,

    /// MODEXP upgrade schedule. Arbitrum One has built-in historical defaults;
    /// other Classic networks need an explicit schedule to enable MODEXP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classic_modexp_upgrades: Option<ClassicModexpUpgrades>,

    /// Arbitrum One has historical defaults. Other Classic chains need this
    /// schedule for builtin behavior that changed during ArbOS upgrades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classic_builtin_upgrades: Option<ClassicBuiltinUpgrades>,

    /// Mirrors Nitro's
    /// `ChainConfig.ArbitrumChainParams.AllowDebugPrecompiles`.
    ///
    /// Defaults to `false`, matching public Arbitrum networks.
    #[serde(default, alias = "AllowDebugPrecompiles")]
    pub allow_debug_precompiles: bool,

    /// ApeChain native yield: account balances come from the yield fields the
    /// Chaintable writer stores under `ape_yield::APE_YIELD_ADDRESS`, and the
    /// ApeChain precompile methods are enabled.
    #[serde(default)]
    pub ape_yield: bool,

    /// Optional override for Nitro's
    /// `ChainConfig.ArbitrumChainParams.GenesisBlockNum`.
    ///
    /// Leave unset for built-in network defaults. Set this for custom Orbit
    /// chains that did not start at L2 block 0.
    #[serde(
        default,
        alias = "GenesisBlockNum",
        skip_serializing_if = "Option::is_none"
    )]
    pub genesis_block_num: Option<u64>,

    /// Mirrors Nitro's `legacy-zero-base-fee-until` header decoder switch.
    ///
    /// Default 0 leaves Nitro's current default behavior unchanged.
    #[serde(
        default,
        alias = "legacyZeroBaseFeeUntil",
        alias = "LegacyZeroBaseFeeUntil",
        alias = "legacy-zero-base-fee-until"
    )]
    pub legacy_zero_base_fee_until: u64,

    /// Nitro `params.ChainConfig` used by ArbOwner.setChainConfig compatibility checks.
    ///
    /// This mirrors `evm.ChainConfig()` in Nitro. It is Arbitrum-specific so the
    /// common RPC/EVM traits do not need to carry chain-config internals.
    #[serde(
        default,
        alias = "chainConfig",
        alias = "ChainConfig",
        skip_serializing_if = "Option::is_none"
    )]
    pub chain_config: Option<Box<RawValue>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classic_mode_is_explicit_and_nitro_remains_default() {
        assert_eq!(
            serde_json::from_str::<ArbitrumEvmConfig>("{}")
                .unwrap()
                .execution_mode,
            ArbitrumExecutionMode::Nitro
        );
        assert_eq!(
            serde_json::from_str::<ArbitrumEvmConfig>(r#"{"execution_mode":"classic"}"#)
                .unwrap()
                .execution_mode,
            ArbitrumExecutionMode::Classic
        );
        assert!(
            serde_json::from_str::<ArbitrumEvmConfig>(r#"{"execution_mode":"clasic"}"#).is_err()
        );
    }
}
