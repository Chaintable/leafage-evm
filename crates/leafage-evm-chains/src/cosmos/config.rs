use leafage_evm_types::Address;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CosmosEvmConfig {
    pub native_token: Option<Address>,
    /// Additional addresses that require execution on the native node.
    #[serde(default)]
    pub unsupported_precompiles: Vec<Address>,
}
