use crate::api_impl::token_collector::TokenCollector;
use alloy::consensus::BlockHeader;
use jsonrpsee::core::RpcResult;
use jsonrpsee::http_client::HttpClient;
use leafage_evm_chains::bsc::BscHardfork;
use leafage_evm_chains::cosmos::{CosmosEvmConfig, CosmosHardfork};
use leafage_evm_chains::mantle::MantleHardfork;
use leafage_evm_types::{BlockEnv, CallRequest, CfgEnv, MainnetSpecId, OpSpecId, H256};
use revm::context::result::{EVMError, InvalidTransaction};
use revm::context::result::{ExecutionResult, HaltReason};
use revm::context::Transaction as TransactionTrait;
use revm::{DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct EvmCfg<SpecId, CustomCfg> {
    pub is_archive: bool,
    pub normalize_state_key: bool,
    pub cfg: CfgEnv<SpecId>,
    pub ovm_address: Option<H256>,
    pub version: String,
    pub estimate_gas_buffer: u64,
    pub custom_cfg: Option<CustomCfg>,
    /// Virtual balance returned by getBalance/getAddressBalance.
    /// When set, all balance queries return this value instead of reading state.
    /// Used by Tempo (no native token — returns sentinel `4242...4242`).
    pub virtual_balance: Option<alloy::primitives::U256>,
}

pub(crate) trait ApiCore: ApiBase + EvmExecutor {}

pub(crate) trait ApiBase: Sync + Send + 'static {
    type DB;
    type SpecId;
    type CustomCfg;

    fn db(&self) -> &Self::DB;

    fn evm_cfg(&self) -> &EvmCfg<Self::SpecId, Self::CustomCfg>;

    fn historical_client(&self) -> Option<&HttpClient>;

    fn historical_height(&self) -> Option<u64>;

    fn token_collector(&self) -> Option<&TokenCollector>;
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

    /// Apply pre-execution changes before transaction execution.
    /// Calls EIP-2935 blockhashes and EIP-4788 beacon root system contracts.
    fn apply_pre_execution_changes<StateDB>(
        &self,
        header: impl BlockHeader,
        block_env: &BlockEnv,
        state: &mut StateDB,
    ) -> RpcResult<()>
    where
        StateDB: DatabaseCommit + DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        Ok(())
    }

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
    Cosmos((CfgEnv<CosmosHardfork>, Option<CosmosEvmConfig>)),
    Mantle(CfgEnv<MantleHardfork>),
    Tempo(CfgEnv<MainnetSpecId>),
}

impl MultiChainCfgEnv {
    pub fn chain_id(&self) -> u64 {
        match self {
            MultiChainCfgEnv::Mainnet(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Op(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Bsc(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Cosmos(cfg) => cfg.0.chain_id,
            MultiChainCfgEnv::Mantle(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Tempo(cfg) => cfg.chain_id,
        }
    }
}
