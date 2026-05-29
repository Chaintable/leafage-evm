use serde::{Deserialize, Serialize};

/// Per-chain configuration for Arbitrum Orbit (Nitro) replicas, parsed from
/// `--evm-custom-config`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ArbitrumEvmConfig {
    /// Replicate Nitro's L1 data-posting cost (posterGas) in `eth_estimateGas`.
    ///
    /// Defaults to `false`: configuring a chain as `arbitrum` does not change
    /// its behaviour (EVM execution stays identical to mainnet, L1 overhead is
    /// always 0) until this is explicitly turned on. Enabled per chain only
    /// after the design-doc gates pass (DAC=false / layout check / genesis state).
    #[serde(default)]
    pub enable_l1_gas: bool,
}
