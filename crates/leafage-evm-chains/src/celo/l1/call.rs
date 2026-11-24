use super::{
    consensus::CeloChainData,
    evm::{create_celo_l1_evm, CeloL1Context},
    fees, system,
    tx::CeloCallTx,
    L1Fork,
};
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId};
use revm::{
    context::result::{EVMError, ExecutionResult, HaltReason},
    database::WrapDatabaseRef,
    handler::Handler,
    inspector::{InspectorHandler, NoOpInspector},
    DatabaseRef, ExecuteEvm, Inspector,
};

pub fn execute<StateDB: DatabaseRef, I>(
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
