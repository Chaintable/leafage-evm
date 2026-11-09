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
    evm::{create_celo_l1_evm, CeloL1Context},
    CeloL1Config, L1Fork,
};
use leafage_evm_storage::BlockIndex;
use leafage_evm_types::{
    BlockId, BlockInfo, BlockNumberOrTag, CallRequest, CfgEnv, MainnetSpecId, U256,
};
use revm::context::{
    result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
    BlockEnv,
};
use revm::{
    database::WrapDatabaseRef, handler::Handler, inspector::InspectorHandler,
    inspector::NoOpInspector, DatabaseCommit, DatabaseRef, ExecuteEvm, Inspector,
};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

type CeloL1ApiImpl<DB> = ApiImpl<DB, MainnetSpecId, CeloL1Config>;
mod fees;
mod system;
mod tx;
use tx::{CeloCallTx, CeloFields};
#[cfg(test)]
mod tests;
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

impl<DB: BlockIndex + Sync + Send + 'static> ApiCore for CeloL1ApiImpl<DB> {
    fn format_block_response(&self, block: &BlockInfo, value: &mut serde_json::Value) {
        if block.header.number < leafage_evm_chains::celo::l1::GINGERBREAD_BLOCK {
            if let Some(object) = value.as_object_mut() {
                for key in ["nonce", "mixHash", "sha3Uncles", "uncles"] {
                    object.remove(key);
                }
            }
        }
    }
}
impl<DB: BlockIndex + Sync + Send + 'static> GasFeeHandler for CeloL1ApiImpl<DB> {
    type Tx = CeloCallTx;

    fn prepare_estimate_request(&self, request: &mut CallRequest) {
        // Native DoEstimateGas clears these before ToMessage validates them.
        request.gas_price = None;
        request.max_fee_per_gas = None;
        request.max_priority_fee_per_gas = None;
        if request.gas.is_some_and(|gas| gas < 21_000) {
            request.gas = None;
        }
    }
}

impl<DB: BlockIndex + Sync + Send + 'static> EvmExecutor for CeloL1ApiImpl<DB> {
    type Tx = CeloCallTx;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        _info: &BlockInfo,
        block: &BlockEnv,
        mut request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        let cfg = CeloL1Config::cfg_at(self.evm_cfg.cfg.clone(), block)
            .map_err(invalid_params_rpc_err)?;
        let fields: CeloFields = serde_json::from_value(
            serde_json::to_value(&request.celo)
                .map_err(|e| invalid_params_rpc_err(e.to_string()))?,
        )
        .map_err(|e| invalid_params_rpc_err(e.to_string()))?;
        if request.gas_price.is_some()
            && (request.max_fee_per_gas.is_some() || request.max_priority_fee_per_gas.is_some())
        {
            return Err(invalid_params_rpc_err(
                "both gasPrice and dynamic fee fields specified",
            ));
        }
        // Native headers have no base fee before Gingerbread. The archive can
        // synthesize one, so use the fork rather than stored field presence.
        let (price, cap, tip) = if let Some(price) = request.gas_price {
            (price, price, price)
        } else if block.number >= U256::from(leafage_evm_chains::celo::l1::GINGERBREAD_BLOCK) {
            let cap = request.max_fee_per_gas.unwrap_or_default();
            let tip = request.max_priority_fee_per_gas.unwrap_or_default();
            (cap.min(tip.saturating_add(block.basefee as u128)), cap, tip)
        } else {
            (0, 0, 0)
        };
        request.gas_price = Some(price);
        request.max_fee_per_gas = None;
        request.max_priority_fee_per_gas = None;
        request.chain_id = Some(chain_id);
        let mut base = create_mainnet_txn_env(block, cfg, request, db, chain_id)?;
        base.tx_type = 0;
        base.gas_priority_fee = None;
        Ok(CeloCallTx {
            base,
            fields,
            fee_cap: U256::from(cap),
            tip_cap: U256::from(tip),
            estimation: false,
        })
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

fn execute<StateDB: DatabaseRef, I>(
    chain: &dyn CeloChainData,
    block: &BlockEnv,
    mut cfg: CfgEnv<MainnetSpecId>,
    state: StateDB,
    tx: CeloCallTx,
    inspector: I,
    inspect: bool,
) -> Result<(ExecutionResult<HaltReason>, revm::state::EvmState), EVMError<StateDB::Error>>
where
    I: Inspector<CeloL1Context<WrapDatabaseRef<StateDB>>>,
{
    let fork = L1Fork::at(block.number.saturating_to()).map_err(EVMError::Custom)?;
    let system = system::System { fork, chain };
    // Match NewSysContractCallCtx's copied state. These configuration queries
    // must not warm the execution's account/storage access list.
    let mut queries = create_celo_l1_evm(
        block.clone(),
        cfg.clone(),
        WrapDatabaseRef(&state),
        NoOpInspector {},
        chain,
    )
    .map_err(EVMError::Custom)?;
    let minimum = system
        .minimum(&mut queries.inner.ctx, tx.fields.fee_currency)
        .map_err(EVMError::Custom)?;
    let (intrinsic, whitelisted) = if let Some(currency) = tx.fields.fee_currency {
        (
            system
                .alternative_intrinsic(&mut queries.inner.ctx)
                .map_err(EVMError::Custom)?,
            system
                .whitelisted(&mut queries.inner.ctx, currency)
                .map_err(EVMError::Custom)?,
        )
    } else {
        (0, true)
    };
    drop(queries);
    cfg.disable_fee_charge = true;
    cfg.disable_base_fee = true;
    cfg.disable_nonce_check = true;
    let mut evm = create_celo_l1_evm(block.clone(), cfg, WrapDatabaseRef(state), inspector, chain)
        .map_err(EVMError::Custom)?;
    evm.inner.ctx.tx = tx.base.clone();
    let mut handler = fees::CallHandler {
        system,
        tx: &tx,
        config: fees::FeeConfig {
            minimum,
            intrinsic,
            whitelisted,
        },
        debit_refund: std::cell::Cell::new(0),
        marker: std::marker::PhantomData,
    };
    let result = if inspect {
        handler.inspect_run(&mut evm)?
    } else {
        handler.run(&mut evm)?
    };
    Ok((result, evm.finalize()))
}
