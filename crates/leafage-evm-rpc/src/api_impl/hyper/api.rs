use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler};
use super::hl_read_precompile::ReadPrecompileForwarder;
use crate::api_impl::mainnet::api::apply_blockhashes_contract_call;
use crate::api_impl::mainnet::evm::{
    create_main_evm_from_state_with_precompiles, create_mainnet_txn_env,
};
use crate::api_impl::ApiImpl;
use alloy::consensus::BlockHeader;
use alloy_evm::precompiles::PrecompilesMap;
use jsonrpsee::core::RpcResult;
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest, MainnetSpecId};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::TxEnv;
use revm::handler::EthPrecompiles;
use revm::inspector::NoOpInspector;
use revm::primitives::hardfork::SpecId;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;
use std::sync::Arc;
use tracing::{info, warn};

pub(crate) type HyperApiImpl<DB> = ApiImpl<DB, MainnetSpecId, HyperEvmConfig>;

/// HyperEVM-specific execution configuration.
#[derive(Clone, Debug, Default)]
pub struct HyperEvmConfig {
    /// Resolves HyperCore read-precompile calls (`0x800..=0x814`) through the upstream.
    /// `None` leaves them as ordinary empty accounts, which is the historical behaviour.
    pub read_precompiles: Option<Arc<ReadPrecompileForwarder>>,
}

impl HyperEvmConfig {
    /// Builds the execution settings for this node, resolving HyperCore read precompiles
    /// through `upstream_url` when `--forward-read-precompiles` is enabled.
    ///
    /// `handle` must be the server's runtime: the forwarder drives its requests on it from
    /// the blocking worker that runs a call.
    pub fn from_upstream(
        forward_read_precompiles: bool,
        upstream_url: Option<&str>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        if !forward_read_precompiles {
            return Self::default();
        }
        let Some(url) = upstream_url else {
            warn!(
                "--forward-read-precompiles needs --upstream-url; read precompiles stay unresolved"
            );
            return Self::default();
        };
        match ReadPrecompileForwarder::new(url, handle) {
            Ok(forwarder) => {
                info!("HyperCore read precompiles (0x800..=0x814) will be resolved through {url}");
                Self {
                    read_precompiles: Some(Arc::new(forwarder)),
                }
            }
            Err(err) => {
                warn!("Failed to set up the HyperCore read precompile forwarder: {err}");
                Self::default()
            }
        }
    }
}

impl<DB> HyperApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    /// Precompiles for an EVM executing at `block_number`: Ethereum's, plus the forwarding
    /// HyperCore read precompiles when an upstream is configured.
    fn precompiles(&self, block_number: u64) -> PrecompilesMap {
        let spec: SpecId = self.evm_cfg.cfg.spec.into();
        match self
            .evm_cfg
            .custom_cfg
            .as_ref()
            .and_then(|cfg| cfg.read_precompiles.as_ref())
        {
            Some(forwarder) => forwarder.precompiles(spec, block_number),
            None => PrecompilesMap::from_static(EthPrecompiles::new(spec).precompiles),
        }
    }
}

impl<DB> GasFeeHandler for HyperApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
}

impl<DB> EvmExecutor for HyperApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)
    }

    /// Backs `eth_call`, `contractMultiCall` and the other call paths.
    fn transact<StateDB: DatabaseRef + Debug>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let precompiles = self.precompiles(block_env.number.saturating_to::<u64>());
        let mut evm = create_main_evm_from_state_with_precompiles(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            NoOpInspector {},
            precompiles,
        );

        evm.transact(tx).map(|res| res.result.into())
    }

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
        apply_blockhashes_contract_call(&self.evm_cfg.cfg, header.parent_hash(), block_env, state)
    }

    /// Backs `simulateTransactions`, `pre_traceCall` and `pre_traceMany`.
    fn inspect_tx_commit<
        StateDB: DatabaseRef + DatabaseCommit + Debug,
        R,
        F: FnOnce(TracingInspector) -> R,
    >(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let precompiles = self.precompiles(block_env.number.saturating_to::<u64>());
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = create_main_evm_from_state_with_precompiles(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            &mut inspector,
            precompiles,
        );

        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}

impl<DB> ApiCore for HyperApiImpl<DB> where DB: Sync + Send + 'static {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_impl::core::Api;
    use crate::{DebankApiServer, EthApiServer};
    use alloy::primitives::TxKind;
    use jsonrpsee::{server::ServerBuilder, types::ErrorObjectOwned, RpcModule};
    use leafage_evm_storage::{BlockContext, BlockIndex, EvmStorageRead, StateDB};
    use leafage_evm_types::{
        Address, BlockId, BlockNumberOrTag, BlockStorageDiff, BlockType, Bytecode, Bytes, CfgEnv,
        DebankBlockContext, H256, U256,
    };
    use std::{
        collections::HashMap,
        convert::Infallible,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    #[derive(Clone, Debug, Default)]
    struct MockState {
        block: BlockInfo,
        accounts: HashMap<H256, revm::state::AccountInfo>,
    }

    impl StateDB for MockState {
        type Error = Infallible;
        fn basic(&self, address: H256) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
            Ok(self.accounts.get(&address).cloned())
        }
        fn code_by_hash(&self, _: H256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }
        fn storage(&self, _: H256, _: H256) -> Result<U256, Self::Error> {
            Ok(U256::ZERO)
        }
        fn block_hash(&self, _: u64) -> Result<H256, Self::Error> {
            Ok(self.block.header.hash)
        }
    }
    impl BlockContext for MockState {
        type Error = Infallible;
        fn block_info(&self) -> Result<BlockInfo, Self::Error> {
            Ok(self.block.clone())
        }
        fn state_diff(&self) -> Result<BlockStorageDiff, Self::Error> {
            Ok(Default::default())
        }
    }
    impl BlockIndex for MockState {
        type Error = Infallible;
        fn get_block_by_id(&self, _: BlockId) -> Result<Option<BlockInfo>, Self::Error> {
            Ok(Some(self.block.clone()))
        }
    }
    impl EvmStorageRead for MockState {
        type Error = Infallible;
        type StateDB = Self;
        fn state_at(&self, _: BlockId) -> Result<Option<Self>, Self::Error> {
            Ok(Some(self.clone()))
        }
    }

    /// `l1BlockNumber`: takes no input and returns one word.
    const L1_BLOCK_NUMBER: Address =
        alloy::primitives::address!("0x0000000000000000000000000000000000000809");

    const L1_BLOCK_NUMBER_OUTPUT: [u8; 32] = [0x2a; 32];

    fn api_with_forwarding(state: MockState, url: &str) -> Api<HyperApiImpl<MockState>> {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
        cfg.chain_id = 999;
        cfg.disable_balance_check = true;
        cfg.disable_eip3607 = true;
        cfg.disable_block_gas_limit = true;
        cfg.disable_base_fee = true;
        cfg.tx_gas_limit_cap = Some(u64::MAX);

        let forwarder =
            ReadPrecompileForwarder::new(url, tokio::runtime::Handle::current()).unwrap();
        Api::new(ApiImpl::new(
            state,
            cfg,
            Some(HyperEvmConfig {
                read_precompiles: Some(Arc::new(forwarder)),
            }),
            None,
            None,
            None,
            false,
            false,
            "test".to_string(),
            0,
            None,
            None,
            None,
        ))
    }

    fn mock_state() -> MockState {
        let mut state = MockState::default();
        state.block.header.number = 7_777_777;
        state.block.header.timestamp = 1_700_000_000;
        state.block.header.hash = H256::repeat_byte(0xaa);
        state.block.header.gas_limit = 30_000_000;
        state
    }

    fn l1_block_number_read() -> CallRequest {
        let mut request = CallRequest::default();
        request.to = Some(TxKind::Call(L1_BLOCK_NUMBER));
        request.gas = Some(100_000);
        request.gas_price = Some(0);
        request
    }

    fn latest_block_ctx() -> DebankBlockContext {
        DebankBlockContext {
            block_id: BlockId::Number(BlockNumberOrTag::Latest),
            block_type: BlockType::Equals,
        }
    }

    /// `simulateTransactions` has to resolve a HyperCore read precompile through the
    /// upstream, price it locally, and show the response in the call trace.
    #[tokio::test(flavor = "multi_thread")]
    async fn simulate_forwards_hypercore_read_precompiles() {
        let requests = Arc::new(AtomicUsize::new(0));
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        let mut module = RpcModule::new(Arc::clone(&requests));
        module
            .register_method("eth_call", |params, requests, _| {
                requests.fetch_add(1, Ordering::SeqCst);
                let (call, tag): (serde_json::Value, String) = params.parse()?;
                assert_eq!(tag, "latest");
                assert_eq!(
                    call["to"].as_str().unwrap_or_default().to_ascii_lowercase(),
                    format!("{L1_BLOCK_NUMBER:#x}")
                );
                Ok::<_, ErrorObjectOwned>(Bytes::from(L1_BLOCK_NUMBER_OUTPUT.to_vec()))
            })
            .unwrap();
        let _handle = server.start(module);

        let api = api_with_forwarding(mock_state(), &url);
        let resp = api
            .simulate_transactions(vec![l1_block_number_read()], Some(latest_block_ctx()), None)
            .await
            .unwrap();

        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(resp.results.len(), 1);
        let result = &resp.results[0];
        assert_eq!(result.code, 0, "{}", result.err);
        // 21000 intrinsic plus the precompile's own 1000 + 33 * 32.
        assert_eq!(result.gas_used, 21_000 + 1000 + 33 * 32);
        assert_eq!(
            result.traces.first().map(|trace| trace.output.clone()),
            Some(Bytes::from(L1_BLOCK_NUMBER_OUTPUT.to_vec()))
        );
    }

    /// `eth_call` and `estimateGas` share the same execution paths as the simulation above,
    /// so they resolve the precompile too, and without a forwarder the call stays empty.
    #[tokio::test(flavor = "multi_thread")]
    async fn call_and_estimate_forward_read_precompiles() {
        let requests = Arc::new(AtomicUsize::new(0));
        let server = ServerBuilder::default().build("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        let mut module = RpcModule::new(Arc::clone(&requests));
        module
            .register_method("eth_call", |_params, requests, _| {
                requests.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ErrorObjectOwned>(Bytes::from(L1_BLOCK_NUMBER_OUTPUT.to_vec()))
            })
            .unwrap();
        let _handle = server.start(module);

        let api = api_with_forwarding(mock_state(), &url);
        let output = api
            .call(
                l1_block_number_read(),
                BlockId::Number(BlockNumberOrTag::Latest),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(output, Bytes::from(L1_BLOCK_NUMBER_OUTPUT.to_vec()));

        let gas = api
            .estimate_gas(l1_block_number_read(), Some(latest_block_ctx()), None)
            .await
            .unwrap();
        // 21000 intrinsic plus the precompile's own 1000 + 33 * 32, plus whatever the binary
        // search's error ratio leaves on top.
        let exact = 21_000 + 1000 + 33 * 32;
        assert!(gas >= U256::from(exact), "{gas} < {exact}");
        assert!(
            gas < U256::from(exact * 102 / 100),
            "{gas} >= {exact} * 1.02"
        );
        // estimateGas re-executes the call on every step of its binary search, and the
        // per-block cache turns those into the one read eth_call already made.
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_forwarder_read_precompiles_stay_empty() {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
        cfg.chain_id = 999;
        cfg.disable_balance_check = true;
        cfg.disable_eip3607 = true;
        cfg.disable_block_gas_limit = true;
        cfg.disable_base_fee = true;
        cfg.tx_gas_limit_cap = Some(u64::MAX);
        let api: Api<HyperApiImpl<MockState>> = Api::new(ApiImpl::new(
            mock_state(),
            cfg,
            Some(HyperEvmConfig::default()),
            None,
            None,
            None,
            false,
            false,
            "test".to_string(),
            0,
            None,
            None,
            None,
        ));

        let output = api
            .call(
                l1_block_number_read(),
                BlockId::Number(BlockNumberOrTag::Latest),
                None,
                None,
            )
            .await
            .unwrap();
        assert!(output.is_empty());
    }
}
