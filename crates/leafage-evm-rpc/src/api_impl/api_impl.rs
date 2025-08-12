use leafage_evm_types::{Address, CfgEnv, SpecId, H256};
use revm::primitives::keccak256;
use std::time::Duration;
use jsonrpsee::http_client::HttpClient;

/// [`ApiImpl`] implements the EthApi trait.
pub struct ApiImpl<DB> {
    pub db: DB,
    pub cfg: CfgEnv<SpecId>,
    pub time_out: Duration,
    pub ovm_address: Option<H256>,
    pub historical_client: Option<HttpClient>,
    pub historical_height: Option<u64>,
}

impl<DB> ApiImpl<DB> {
    pub fn new(
        db: DB,
        cfg: CfgEnv<SpecId>,
        time_out: Duration,
        ovm_address: Option<Address>,
        historical_client: Option<HttpClient>,
        historical_height: Option<u64>,
    ) -> Self {
        Self {
            db,
            cfg,
            time_out,
            ovm_address: ovm_address.map(|addr| keccak256(addr.as_slice())),
            historical_client,
            historical_height,
        }
    }
}
