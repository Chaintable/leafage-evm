use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// Per-chain configuration for Arbitrum Orbit (Nitro) replicas, parsed from
/// `--evm-custom-config`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ArbitrumEvmConfig {
    /// Mirrors Nitro's
    /// `ChainConfig.ArbitrumChainParams.AllowDebugPrecompiles`.
    ///
    /// Defaults to `false`, matching public Arbitrum networks.
    #[serde(default, alias = "AllowDebugPrecompiles")]
    pub allow_debug_precompiles: bool,

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
