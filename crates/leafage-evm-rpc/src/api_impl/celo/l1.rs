use crate::{
    api_impl::{
        core::{ApiCore, EvmExecutor, GasFeeHandler},
        mainnet::evm::create_mainnet_txn_env,
        ApiImpl,
    },
    error::invalid_params_rpc_err,
};
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::celo::l1::{
    consensus::{CeloChainData, CeloHeader},
    evm::create_celo_l1_evm,
    CeloL1Config,
};
use leafage_evm_storage::BlockIndex;
use leafage_evm_types::{BlockId, BlockInfo, BlockNumberOrTag, CallRequest, MainnetSpecId};
use revm::context::{
    result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
    BlockEnv, TxEnv,
};
use revm::{
    database::WrapDatabaseRef, inspector::NoOpInspector, DatabaseCommit, DatabaseRef, ExecuteEvm,
    InspectCommitEvm,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

type CeloL1ApiImpl<DB> = ApiImpl<DB, MainnetSpecId, CeloL1Config>;
pub(super) struct ArchiveChain<'a, DB>(pub &'a DB);
impl<DB: BlockIndex> CeloChainData for ArchiveChain<'_, DB> {
    fn header(&self, number: u64) -> Result<CeloHeader, String> {
        self.0
            .get_block_by_id(BlockId::Number(BlockNumberOrTag::Number(number)))
            .map_err(|e| e.to_string())?
            .map(|b| CeloHeader::from_header(&b.header))
            .ok_or_else(|| format!("Celo L1 consensus header {number} is unavailable"))
    }
}

impl<DB: BlockIndex + Sync + Send + 'static> ApiCore for CeloL1ApiImpl<DB> {}
impl<DB: BlockIndex + Sync + Send + 'static> GasFeeHandler for CeloL1ApiImpl<DB> {
    type Tx = TxEnv;
}

impl<DB: BlockIndex + Sync + Send + 'static> EvmExecutor for CeloL1ApiImpl<DB> {
    type Tx = TxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        _: &BlockInfo,
        block: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        let cfg = CeloL1Config::cfg_at(self.evm_cfg.cfg.clone(), block)
            .map_err(invalid_params_rpc_err)?;
        create_mainnet_txn_env(block, cfg, request, db, chain_id)
    }

    fn transact<StateDB: DatabaseRef>(
        &self,
        block: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<ExecutionResult<HaltReason>, EVMError<StateDB::Error>> {
        let chain = ArchiveChain(&self.db);
        let mut evm = create_celo_l1_evm(
            block.clone(),
            self.evm_cfg.cfg.clone(),
            WrapDatabaseRef(state),
            NoOpInspector {},
            &chain,
        )
        .map_err(EVMError::Custom)?;
        evm.transact(tx).map(|r| r.result)
    }

    fn inspect_tx_commit<
        StateDB: DatabaseRef + DatabaseCommit,
        R,
        F: FnOnce(TracingInspector) -> R,
    >(
        &self,
        block: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        collect: F,
        tx: Self::Tx,
    ) -> Result<(ExecutionResult<HaltReason>, R), EVMError<StateDB::Error>> {
        let chain = ArchiveChain(&self.db);
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = create_celo_l1_evm(
            block.clone(),
            self.evm_cfg.cfg.clone(),
            WrapDatabaseRef(state),
            &mut inspector,
            &chain,
        )
        .map_err(EVMError::Custom)?;
        evm.inspect_tx_commit(tx).map(|r| (r, collect(inspector)))
    }
}
