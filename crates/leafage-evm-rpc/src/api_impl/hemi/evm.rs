use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::hemi::{HemiEvm, HemiHardfork};
use leafage_evm_types::{BlockEnv, CallRequest, CfgEnv, OpSpecId};
use op_revm::{precompiles::OpPrecompiles, L1BlockInfo, OpBuilder, OpTransaction};
use revm::context::TxEnv;
use revm::database::{DatabaseRef, WrapDatabaseRef};
use revm::Context;

pub(crate) fn create_hemi_txn_env<ODB: DatabaseRef, SpecId>(
    block_env: &BlockEnv,
    cfg_env: CfgEnv<SpecId>,
    request: CallRequest,
    db: ODB,
    origin_chain_id: u64,
) -> RpcResult<OpTransaction<TxEnv>> {
    let base = create_mainnet_txn_env(block_env, cfg_env, request, db, origin_chain_id)?;
    Ok(OpTransaction {
        base,
        enveloped_tx: Some(leafage_evm_types::Bytes::new()),
        deposit: Default::default(),
    })
}

pub(crate) fn create_hemi_evm_from_state<StateDB, INSP>(
    block_env: BlockEnv,
    cfg: CfgEnv<HemiHardfork>,
    state: StateDB,
    inspector: INSP,
) -> HemiEvm<WrapDatabaseRef<StateDB>, INSP>
where
    StateDB: DatabaseRef,
{
    let inner = Context::op()
        .with_block(block_env)
        .with_cfg(HemiHardfork::convert_cfg_env(cfg))
        .with_ref_db(state)
        .build_op_with_inspector(inspector)
        .with_precompiles(OpPrecompiles::default());

    HemiEvm::new(inner, false)
}
