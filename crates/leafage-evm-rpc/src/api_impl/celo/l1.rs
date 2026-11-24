use crate::{
    api_impl::{
        core::{ApiCore, EvmExecutor, GasFeeHandler, TxSetter},
        ApiImpl,
    },
    error::{database_rpc_err, invalid_params_rpc_err},
};
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::celo::l1::{
    call::execute,
    consensus::{CeloChainData, CeloHeader},
    tx::{self, CallInputError, CeloCallTx},
    CeloL1Config,
};
use leafage_evm_storage::BlockIndex;
use leafage_evm_types::{BlockId, BlockInfo, BlockNumberOrTag, CallRequest, MainnetSpecId};
use revm::context::{
    result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
    BlockEnv,
};
use revm::{inspector::NoOpInspector, DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

type CeloL1ApiImpl<DB> = ApiImpl<DB, MainnetSpecId, CeloL1Config>;
pub(crate) mod rpc;

fn input_error<E: std::error::Error>(
    error: CallInputError<E>,
) -> jsonrpsee::types::ErrorObjectOwned {
    match error {
        CallInputError::InvalidParams(message) => invalid_params_rpc_err(message),
        CallInputError::Database(error) => database_rpc_err(format!("get nonce failed: {error}")),
    }
}

impl TxSetter for CeloCallTx {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        CeloCallTx::set_gas_limit(self, gas_limit);
    }
    fn set_gas_estimation(&mut self) {
        CeloCallTx::set_gas_estimation(self);
    }
}

struct ArchiveChain<'a, DB>(&'a DB);
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
    type Tx = CeloCallTx;
}

impl<DB: BlockIndex + Sync + Send + 'static> EvmExecutor for CeloL1ApiImpl<DB> {
    type Tx = CeloCallTx;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        _info: &BlockInfo,
        block: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        tx::create_txn_env(block, self.evm_cfg.cfg.clone(), request, db, chain_id)
            .map_err(input_error)
    }

    fn transact<StateDB: DatabaseRef>(
        &self,
        block: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<ExecutionResult<HaltReason>, EVMError<StateDB::Error>> {
        let chain = ArchiveChain(&self.db);
        execute(
            &chain,
            block,
            self.evm_cfg.cfg.clone(),
            state,
            tx,
            NoOpInspector {},
            false,
        )
        .map(|r| r.0)
    }

    fn inspect_tx_commit<
        StateDB: DatabaseRef + DatabaseCommit,
        R,
        F: FnOnce(TracingInspector) -> R,
    >(
        &self,
        block: &BlockEnv,
        mut state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        collect: F,
        tx: Self::Tx,
    ) -> Result<(ExecutionResult<HaltReason>, R), EVMError<StateDB::Error>> {
        let chain = ArchiveChain(&self.db);
        let mut inspector = TracingInspector::new(inspector_cfg);
        let (result, changes) = execute(
            &chain,
            block,
            self.evm_cfg.cfg.clone(),
            &state,
            tx,
            &mut inspector,
            true,
        )?;
        state.commit(changes);
        Ok((result, collect(inspector)))
    }
}
