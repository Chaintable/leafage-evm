use leafage_evm_types::{Address};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CosmosEvmConfig {
    pub native_token: Option<Address>,
}

impl Default for CosmosEvmConfig {
    fn default() -> Self {
        Self { native_token: None }
    }
}
