use jsonrpsee::core::RpcResult;
use jsonrpsee::http_client::HttpClient;
use leafage_evm_chains::bsc::BscHardfork;
use leafage_evm_types::{BlockEnv, CallRequest, CfgEnv, MainnetSpecId, OpSpecId, H256};
use revm::context::result::{EVMError, InvalidTransaction};
use revm::context::result::{ExecutionResult, HaltReason};
use revm::context::Transaction as TransactionTrait;
use revm::{DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct EvmCfg<SpecId> {
    pub is_archive: bool,
    pub normalize_state_key: bool,
    pub cfg: CfgEnv<SpecId>,
    pub ovm_address: Option<H256>,
    pub time_out: Duration,
}

pub(crate) trait ApiCore: ApiBase + EvmExecutor {}

pub(crate) trait ApiBase: Sync + Send + 'static {
    type DB;
    type SpecId;

    fn db(&self) -> &Self::DB;

    fn evm_cfg(&self) -> &EvmCfg<Self::SpecId>;

    fn historical_client(&self) -> Option<&HttpClient>;

    fn historical_height(&self) -> Option<u64>;
}

pub(crate) trait EvmExecutor: Sync + Send + 'static {
    type Tx: TxSetter + TransactionTrait + Clone;

    type TransactionError: ToJsonRpcError + GetTransactionError;

    type EvmHaltReason: std::fmt::Debug + Clone;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx>;

    fn transact<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static;

    fn inspect_tx_commit<StateDB, R, F>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseCommit + DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
        F: FnOnce(TracingInspector) -> R;
}

pub(crate) trait TxSetter {
    fn set_gas_limit(&mut self, gas_limit: u64);
}

pub(crate) trait ToJsonRpcError: std::fmt::Display {
    fn to_rpc_error(&self) -> jsonrpsee::types::ErrorObjectOwned;
}

pub(crate) trait GetTransactionError {
    fn get_transaction_error(&self) -> Option<InvalidTransaction>;
}

pub(crate) trait GetHaltReason {
    fn get_halt_reason(&self) -> Option<HaltReason>;
}

pub(crate) struct Api<C> {
    pub inner: Arc<C>,
}

impl<C> Clone for Api<C> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Clone, Debug)]
pub enum MultiChainCfgEnv {
    Mainnet(CfgEnv<MainnetSpecId>),
    Op(CfgEnv<OpSpecId>),
    Bsc(CfgEnv<BscHardfork>),
}

impl From<(u64, String)> for MultiChainCfgEnv {
    fn from((chain_id, evm_type): (u64, String)) -> Self {
        match evm_type.as_str() {
            "mainnet" => {
                let mut chain_cfg = CfgEnv::default();
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                MultiChainCfgEnv::Mainnet(chain_cfg)
            }
            "op" => {
                let mut chain_cfg = CfgEnv::default();
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                MultiChainCfgEnv::Op(chain_cfg)
            }
            "bsc" => {
                let mut chain_cfg = CfgEnv::default();
                chain_cfg.disable_balance_check = true;
                chain_cfg.disable_eip3607 = true;
                chain_cfg.disable_block_gas_limit = true;
                chain_cfg.disable_base_fee = true;
                chain_cfg.chain_id = chain_id;
                MultiChainCfgEnv::Bsc(chain_cfg)
            }
            _ => panic!("Unsupported evm type"),
        }
    }
}

impl MultiChainCfgEnv {
    pub fn chain_id(&self) -> u64 {
        match self {
            MultiChainCfgEnv::Mainnet(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Op(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Bsc(cfh) => cfh.chain_id,
        }
    }
}