use crate::api_impl::token_collector::TokenCollector;
use alloy::consensus::BlockHeader;
use jsonrpsee::{core::RpcResult, http_client::HttpClient};
use leafage_evm_chains::arbitrum::{ArbitrumEvmConfig, ArbitrumHardfork};
use leafage_evm_chains::base::BaseHardfork;
use leafage_evm_chains::bsc::BscHardfork;
use leafage_evm_chains::citrea::CitreaHardfork;
use leafage_evm_chains::cosmos::{CosmosEvmConfig, CosmosHardfork};
use leafage_evm_chains::iotex::IotexHardfork;
use leafage_evm_chains::mantle::MantleHardfork;
use leafage_evm_chains::moonbeam::MoonbeamHardfork;
use leafage_evm_chains::polygon::PolygonHardfork;
use leafage_evm_chains::tempo::hardfork::TempoHardfork;
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest, CfgEnv, MainnetSpecId, OpSpecId, H256};
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
    /// Per-server limiter for CPU-bound EVM execution (call / multicall /
    /// estimateGas / simulate / trace). `None` keeps execution unbounded.
    pub exec_limiter: Option<Arc<tokio::sync::Semaphore>>,
}

pub(crate) trait ApiCore:
    ApiBase + EvmExecutor + GasFeeHandler<Tx = <Self as EvmExecutor>::Tx>
{
}

pub(crate) trait ApiBase: Sync + Send + 'static {
    type DB;
    type SpecId: Into<revm::primitives::hardfork::SpecId> + Clone;
    type CustomCfg;

    fn db(&self) -> &Self::DB;

    fn evm_cfg(&self) -> &EvmCfg<Self::SpecId, Self::CustomCfg>;

    fn historical_client(&self) -> Option<&HttpClient>;

    fn historical_height(&self) -> Option<u64>;

    fn token_collector(&self) -> Option<&TokenCollector>;
}

pub(crate) trait GasFeeHandler: Sync + Send + 'static {
    type Tx: TxSetter + TransactionTrait + Clone;
    fn virtual_balance(&self) -> Option<alloy::primitives::U256> {
        None
    }

    fn gas_allowance<StateDB: DatabaseRef>(
        &self,
        _request: &CallRequest,
        tx: &Self::Tx,
        db: &StateDB,
        _block_env: &BlockEnv,
    ) -> RpcResult<u64> {
        use crate::error::rpc_error_with_code;
        use leafage_evm_types::DebankErrorCode;

        let caller = db.basic_ref(tx.caller()).map_err(|e| {
            rpc_error_with_code(DebankErrorCode::DataBaseFailed as i32, e.to_string())
        })?;
        let balance = caller
            .map(|acc| acc.balance)
            .unwrap_or_default()
            .checked_sub(tx.value())
            .ok_or_else(|| {
                rpc_error_with_code(
                    DebankErrorCode::BalanceExhausted as i32,
                    "Insufficient funds".to_string(),
                )
            })?;
        Ok(balance
            .checked_div(alloy::primitives::U256::from(tx.gas_price()))
            .unwrap_or_default()
            .try_into()
            .unwrap())
    }

    fn estimate_l1_overhead<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        _block_env: &BlockEnv,
        _tx: Self::Tx,
        _state: &StateDB,
    ) -> u64
    where
        StateDB::Error: Sync + Send + 'static,
        StateDB: Debug,
    {
        0
    }
}

pub(crate) trait EvmExecutor: Sync + Send + 'static {
    type Tx: TxSetter + TransactionTrait + Clone;

    type TransactionError: ToJsonRpcError + GetTransactionError;

    type EvmHaltReason: std::fmt::Debug + Clone;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx>;

    fn apply_pre_execution_changes<StateDB>(
        &self,
        _header: impl BlockHeader,
        _block_env: &BlockEnv,
        _state: &mut StateDB,
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

    /// Mark this transaction as a gas-estimation run. Chains whose gas
    /// accounting depends on the run mode (Arbitrum's L1 poster padding)
    /// override this; the default is a no-op.
    fn set_gas_estimation(&mut self) {}
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
    Arbitrum((CfgEnv<ArbitrumHardfork>, Option<ArbitrumEvmConfig>)),
    Op(CfgEnv<OpSpecId>),
    Base(CfgEnv<BaseHardfork>),
    Bsc(CfgEnv<BscHardfork>),
    Cosmos((CfgEnv<CosmosHardfork>, Option<CosmosEvmConfig>)),
    Iotex(CfgEnv<IotexHardfork>),
    Mantle(CfgEnv<MantleHardfork>),
    Moonbeam(CfgEnv<MoonbeamHardfork>),
    Polygon(CfgEnv<PolygonHardfork>),
    Tempo(CfgEnv<TempoHardfork>),
    Citrea(CfgEnv<CitreaHardfork>),
}

impl MultiChainCfgEnv {
    pub fn chain_id(&self) -> u64 {
        match self {
            MultiChainCfgEnv::Mainnet(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Arbitrum(cfg) => cfg.0.chain_id,
            MultiChainCfgEnv::Op(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Base(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Bsc(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Cosmos(cfg) => cfg.0.chain_id,
            MultiChainCfgEnv::Iotex(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Mantle(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Moonbeam(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Polygon(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Tempo(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Citrea(cfg) => cfg.chain_id,
        }
    }
}
