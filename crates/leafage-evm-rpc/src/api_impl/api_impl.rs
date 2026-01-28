use crate::api_impl::core::{ApiBase, EvmCfg, GetTransactionError, ToJsonRpcError};
use crate::error::rpc_error_with_code;
use jsonrpsee::http_client::HttpClient;
use leafage_evm_types::{Address, DebankErrorCode};
use revm::context::result::{EVMError, InvalidTransaction};
use revm::context::CfgEnv;
use revm::primitives::keccak256;
use std::str::FromStr;
use std::time::Duration;

/// [`ApiImpl`] implements the EthApi trait.
pub struct ApiImpl<DB, SpecId, CustomCfg> {
    pub db: DB,
    pub evm_cfg: EvmCfg<SpecId, CustomCfg>,
    pub historical_client: Option<HttpClient>,
    pub historical_height: Option<u64>,
}

impl<DB, SpecId, CustomCfg> ApiImpl<DB, SpecId, CustomCfg> {
    pub fn new(
        db: DB,
        cfg: CfgEnv<SpecId>,
        custom_cfg: Option<CustomCfg>,
        time_out: Duration,
        ovm_address: Option<Address>,
        historical_client: Option<HttpClient>,
        historical_height: Option<u64>,
        is_archive: bool,
        normalize_state_key: bool,
        version: String,
        estimate_gas_buffer: u64,
    ) -> Self {
        Self {
            db,
            evm_cfg: EvmCfg {
                is_archive,
                normalize_state_key,
                cfg,
                ovm_address: ovm_address.map(|addr| keccak256(addr.as_slice())),
                time_out,
                version,
                estimate_gas_buffer,
                custom_cfg,
            },
            historical_client,
            historical_height,
        }
    }
}

impl<DB, SpecId, CustomCfg> ApiBase for ApiImpl<DB, SpecId, CustomCfg>
where
    DB: Sync + Send + 'static,
    SpecId: Send + Sync + 'static,
    CustomCfg: Send + Sync + 'static,
{
    type DB = DB;
    type SpecId = SpecId;
    type CustomCfg = CustomCfg;

    fn db(&self) -> &Self::DB {
        &self.db
    }

    fn evm_cfg(&self) -> &EvmCfg<Self::SpecId, Self::CustomCfg> {
        &self.evm_cfg
    }

    fn historical_client(&self) -> Option<&HttpClient> {
        self.historical_client.as_ref()
    }

    fn historical_height(&self) -> Option<u64> {
        self.historical_height
    }
}

impl<DBError, T> ToJsonRpcError for EVMError<DBError, T>
where
    DBError: std::error::Error,
    T: ToJsonRpcError + std::fmt::Display,
{
    fn to_rpc_error(&self) -> jsonrpsee::types::ErrorObjectOwned {
        match self {
            e => match e {
                EVMError::Database(e) => {
                    rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
                }
                EVMError::Header(e) => {
                    rpc_error_with_code(DebankErrorCode::InvalidParams as i32, e.to_string())
                }
                EVMError::Transaction(t) => t.to_rpc_error(),

                EVMError::Custom(str) if str.starts_with("unsupported precompile address: ") => {
                    if let Some(address) = str.split(": ").nth(1) {
                        if let Ok(_) = Address::from_str(address) {
                            return rpc_error_with_code(
                                DebankErrorCode::UnsupportedPrecompile as i32,
                                e.to_string(),
                            );
                        }
                    }
                    rpc_error_with_code(DebankErrorCode::EvmFailed as i32, e.to_string())
                }
                _ => rpc_error_with_code(DebankErrorCode::EvmFailed as i32, e.to_string()),
            },
        }
    }
}

impl<T: GetTransactionError, DBError> GetTransactionError for EVMError<DBError, T> {
    fn get_transaction_error(&self) -> Option<InvalidTransaction> {
        match self {
            EVMError::Transaction(t) => t.get_transaction_error(),
            _ => None,
        }
    }
}

/// NoneEvmCustomConfig represents an EVM configuration without any additional customization.
#[derive(Debug, Clone)]
pub struct NoneEvmCustomConfig;
