use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CitreaEvmConfig {
    pub l1_fee_rate: u128,
}

impl Default for CitreaEvmConfig {
    fn default() -> Self {
        Self { l1_fee_rate: 0 }
    }
}
