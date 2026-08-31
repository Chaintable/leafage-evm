use crate::api_impl::token_collector::TokenCollector;
use crate::error::internal_rpc_err;
use alloy::consensus::BlockHeader;
use alloy::rpc::types::state::StateOverride;
use alloy::sol_types::decode_revert_reason;
use jsonrpsee::core::RpcResult;
use jsonrpsee::http_client::HttpClient;
use leafage_evm_chains::arbitrum::{ArbitrumEvmConfig, ArbitrumHardfork};
use leafage_evm_chains::arc::ArcChainConfig;
use leafage_evm_chains::base::BaseHardfork;
use leafage_evm_chains::bsc::BscHardfork;
use leafage_evm_chains::citrea::CitreaHardfork;
use leafage_evm_chains::cosmos::{CosmosEvmConfig, CosmosHardfork};
use leafage_evm_chains::hemi::HemiHardfork;
use leafage_evm_chains::iotex::IotexHardfork;
use leafage_evm_chains::mantle::MantleHardfork;
use leafage_evm_chains::moonbeam::MoonbeamHardfork;
use leafage_evm_chains::polygon::PolygonHardfork;
use leafage_evm_chains::tempo::hardfork::TempoHardfork;
use leafage_evm_types::{
    block_env_from_block, BlockEnv, BlockInfo, BlockOverrides, Bytes, CallRequest, CfgEnv,
    DebankEvent, DebankTrace, Header, MainnetSpecId, OpSpecId, H256,
};
use revm::bytecode::OpCode;
use revm::context::result::{EVMError, InvalidTransaction};
use revm::context::result::{ExecutionResult, HaltReason};
use revm::context::Transaction as TransactionTrait;
use revm::database::CacheDB;
use revm::primitives::{eip7825, hardfork::SpecId as EthSpecId};
use revm::{DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{OpcodeFilter, TracingInspector, TracingInspectorConfig};
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
    /// Per-server limiter for plain state reads (getAddressCode /
    /// getStorageAt / nonce / balance and blockx_stateReadBatch), kept
    /// separate from the EVM limiter: reads are disk-bound and must not
    /// starve — or be starved by — CPU-bound execution. `None` keeps
    /// reads unbounded.
    pub state_read_limiter: Option<Arc<tokio::sync::Semaphore>>,
}

pub(crate) struct PreparedSimulationEnvironment {
    pub(crate) block_env: BlockEnv,
    pub(crate) pre_execution_header: Option<Header>,
}

pub(crate) struct SimulationExecutionOutput<R> {
    pub(crate) result: ExecutionResult<R>,
    pub(crate) traces: Vec<DebankTrace>,
    pub(crate) events: Vec<DebankEvent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StateOverrideEndpoint {
    EthCall,
    DebankCall,
}

impl PreparedSimulationEnvironment {
    pub(crate) fn generic<DB>(
        block: &BlockInfo,
        overrides: Option<BlockOverrides>,
        db: &mut CacheDB<DB>,
    ) -> Self {
        let mut block_env = block_env_from_block(block);
        let pre_execution_header = overrides.and_then(|overrides| {
            super::utils::apply_block_overrides(overrides, db, &mut block_env, block.header.clone())
        });
        Self {
            block_env,
            pre_execution_header,
        }
    }
}

fn default_call_result<R: Debug>(result: ExecutionResult<R>) -> RpcResult<Bytes> {
    match result {
        ExecutionResult::Success { output, .. } => Ok(output.into_data().0.into()),
        ExecutionResult::Revert { output, .. } => Err(internal_rpc_err(format!(
            "Reverted: {:?}",
            decode_revert_reason(&output).unwrap_or("execution revert".to_string())
        ))),
        ExecutionResult::Halt { reason, gas, .. } => Err(internal_rpc_err(format!(
            "Halted: {:?} {}",
            reason,
            gas.used()
        ))),
    }
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

    fn consensus_tx_gas_limit_cap(&self, spec: EthSpecId) -> u64 {
        if spec.is_enabled_in(EthSpecId::OSAKA) {
            eip7825::TX_GAS_LIMIT_CAP
        } else {
            u64::MAX
        }
    }

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

    fn estimate_l1_overhead<StateDB>(
        &self,
        _block: &BlockInfo,
        _block_env: &BlockEnv,
        _tx: Self::Tx,
        _state: &StateDB,
    ) -> u64
    where
        StateDB: DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        0
    }
}

pub(crate) trait EvmExecutor: Sync + Send + 'static {
    type Tx: TxSetter + TransactionTrait + Clone;

    type TransactionError: ToJsonRpcError + GetTransactionError;

    type EvmHaltReason: std::fmt::Debug + Clone;

    fn prepare_simulation_environment<DB>(
        &self,
        block: &BlockInfo,
        overrides: Option<BlockOverrides>,
        db: &mut CacheDB<DB>,
    ) -> RpcResult<PreparedSimulationEnvironment> {
        Ok(PreparedSimulationEnvironment::generic(block, overrides, db))
    }

    fn apply_state_overrides<DB>(
        &self,
        endpoint: StateOverrideEndpoint,
        overrides: StateOverride,
        db: &mut CacheDB<DB>,
    ) -> RpcResult<()>
    where
        DB: DatabaseRef,
    {
        let _ = endpoint;
        super::utils::apply_state_overrides(overrides, db)
    }

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx>;

    fn create_txn_env_for_call<StateDB: DatabaseRef>(
        &self,
        block: &BlockInfo,
        block_env: BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<(BlockEnv, Self::Tx)> {
        let tx = self.create_txn_env(block, &block_env, request, db, chain_id)?;
        Ok((block_env, tx))
    }

    /// Prepares an `eth_call` transaction. Other call-like APIs continue to
    /// use `create_txn_env_for_call`, so Arc can expose Reth error codes on
    /// `eth_call` without changing DeBank multicall contracts.
    fn create_txn_env_for_eth_call<StateDB: DatabaseRef>(
        &self,
        block: &BlockInfo,
        block_env: BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<(BlockEnv, Self::Tx)> {
        self.create_txn_env_for_call(block, block_env, request, db, chain_id)
    }

    fn create_txn_env_for_simulation<StateDB: DatabaseRef>(
        &self,
        block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        self.create_txn_env(block, block_env, request, db, chain_id)
    }

    /// Maps an EVM error produced by `eth_call`. Generic executors preserve
    /// Leafage's existing error contract; Arc overrides this with
    /// Reth-compatible call errors.
    fn call_error<DBError>(
        &self,
        error: &EVMError<DBError, Self::TransactionError>,
    ) -> jsonrpsee::types::ErrorObjectOwned
    where
        DBError: std::error::Error,
    {
        error.to_rpc_error()
    }

    /// Converts the completed `eth_call` result into its RPC response.
    /// Generic executors preserve Leafage's existing error contract; Arc
    /// overrides this with Reth-compatible execution errors.
    fn call_result(&self, result: ExecutionResult<Self::EvmHaltReason>) -> RpcResult<Bytes> {
        default_call_result(result)
    }

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

    #[allow(clippy::type_complexity)]
    fn transact_for_call<StateDB>(
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
        StateDB::Error: Sync + Send + 'static,
    {
        self.transact(block_env, state, tx)
    }

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

    fn execute_simulation<StateDB>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx_hash: H256,
        tx: Self::Tx,
    ) -> Result<
        SimulationExecutionOutput<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseCommit + DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        let mut inspector_cfg = TracingInspectorConfig::default_parity()
            .set_record_logs(true)
            .set_steps(true);
        inspector_cfg.record_opcodes_filter = Some(OpcodeFilter::new().enabled(OpCode::SSTORE));

        let (result, traces) = self.inspect_tx_commit(
            block_env,
            state,
            inspector_cfg,
            |inspector| inspector.into_traces(),
            tx,
        )?;
        let (traces, events) = super::utils::build_debank_traces(tx_hash, traces);

        Ok(SimulationExecutionOutput {
            result,
            traces,
            events,
        })
    }
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
    Arc((CfgEnv<MainnetSpecId>, ArcChainConfig)),
    Arbitrum((CfgEnv<ArbitrumHardfork>, Option<ArbitrumEvmConfig>)),
    Op(CfgEnv<OpSpecId>),
    Base(CfgEnv<BaseHardfork>),
    Bsc(CfgEnv<BscHardfork>),
    Cosmos((CfgEnv<CosmosHardfork>, Option<CosmosEvmConfig>)),
    Iotex(CfgEnv<IotexHardfork>),
    Mantle(CfgEnv<MantleHardfork>),
    Moonbeam(CfgEnv<MoonbeamHardfork>),
    Polygon(CfgEnv<PolygonHardfork>),
    Hemi(CfgEnv<HemiHardfork>),
    Tempo(CfgEnv<TempoHardfork>),
    Citrea(CfgEnv<CitreaHardfork>),
}

impl MultiChainCfgEnv {
    pub fn chain_id(&self) -> u64 {
        match self {
            MultiChainCfgEnv::Mainnet(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Arc(cfg) => cfg.0.chain_id,
            MultiChainCfgEnv::Arbitrum(cfg) => cfg.0.chain_id,
            MultiChainCfgEnv::Op(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Base(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Bsc(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Cosmos(cfg) => cfg.0.chain_id,
            MultiChainCfgEnv::Iotex(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Mantle(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Moonbeam(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Polygon(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Hemi(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Tempo(cfg) => cfg.chain_id,
            MultiChainCfgEnv::Citrea(cfg) => cfg.chain_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context::TxEnv;

    struct DefaultGasFeeHandler;

    impl GasFeeHandler for DefaultGasFeeHandler {
        type Tx = TxEnv;
    }

    #[test]
    fn default_consensus_cap_keeps_mainnet_eip7825_boundary() {
        let handler = DefaultGasFeeHandler;
        assert_eq!(
            handler.consensus_tx_gas_limit_cap(EthSpecId::PRAGUE),
            u64::MAX
        );
        assert_eq!(
            handler.consensus_tx_gas_limit_cap(EthSpecId::OSAKA),
            eip7825::TX_GAS_LIMIT_CAP
        );
    }

    #[test]
    fn arc_config_keeps_its_own_chain_variant() {
        let config = ArcChainConfig::mainnet();
        let mut cfg = CfgEnv::new_with_spec(config.ethereum_spec());
        cfg.chain_id = config.chain_id();

        let multi_chain = MultiChainCfgEnv::Arc((cfg, config));
        assert_eq!(multi_chain.chain_id(), 5042);
        assert!(matches!(multi_chain, MultiChainCfgEnv::Arc(_)));
    }

    #[test]
    fn default_call_result_keeps_the_existing_internal_revert_error() {
        let error = default_call_result::<HaltReason>(ExecutionResult::Revert {
            gas: revm::context::result::ResultGas::new(30_000, 21_000, 0, 0, 21_000),
            logs: Vec::new(),
            output: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        })
        .unwrap_err();

        assert_eq!(error.code(), -32603);
        assert_eq!(error.message(), "Reverted: \"execution revert\"");
        assert!(error.data().is_none());
    }
}
