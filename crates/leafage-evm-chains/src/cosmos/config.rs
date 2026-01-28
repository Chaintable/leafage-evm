use leafage_evm_types::{Address, U256};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CosmosEvmConfig {
    pub native_token: Option<TokenConfig>,
}

impl Default for CosmosEvmConfig {
    fn default() -> Self {
        Self { native_token: None }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenConfig {
    pub address: Address,
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    pub total_supply: U256,
}
