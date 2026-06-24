use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::base::BaseHardfork;
use leafage_evm_types::{BlockEnv, CallRequest, CfgEnv, OpSpecId};
use op_revm::{
    precompiles::OpPrecompiles, DefaultOp, L1BlockInfo, OpBuilder, OpEvm, OpTransaction,
};
use revm::context::TxEnv;
use revm::database::{DatabaseRef, WrapDatabaseRef};
use revm::handler::instructions::EthInstructions;
use revm::interpreter::interpreter::EthInterpreter;
use revm::Context;

pub(crate) fn create_base_txn_env<ODB: DatabaseRef, SpecId>(
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

pub(crate) fn create_base_evm_from_state<StateDB, INSP>(
    block_env: BlockEnv,
    cfg: CfgEnv<BaseHardfork>,
    state: StateDB,
    inspector: INSP,
) -> OpEvm<
    Context<
        BlockEnv,
        OpTransaction<TxEnv>,
        CfgEnv<OpSpecId>,
        WrapDatabaseRef<StateDB>,
        revm::Journal<WrapDatabaseRef<StateDB>>,
        L1BlockInfo,
    >,
    INSP,
    EthInstructions<
        EthInterpreter,
        Context<
            BlockEnv,
            OpTransaction<TxEnv>,
            CfgEnv<OpSpecId>,
            WrapDatabaseRef<StateDB>,
            revm::Journal<WrapDatabaseRef<StateDB>>,
            L1BlockInfo,
        >,
    >,
>
where
    StateDB: DatabaseRef,
{
    // NOTE (Stage 2 WIP): the B20 read precompiles + `extend_base_precompiles`
    // are implemented and unit-tested in `leafage_evm_chains::base::b20`, but
    // wiring an alloy-evm `PrecompilesMap` into op-revm's `OpEvm` hit an
    // unresolved `PrecompileProvider`/`ExecuteEvm` trait-bound mismatch (revm is
    // unified at 36, so it is not a version skew). Until that is resolved, base
    // uses the op precompile set (base ≡ op for execution).
    Context::op()
        .with_block(block_env)
        .with_cfg(BaseHardfork::convert_cfg_env(cfg))
        .with_ref_db(state)
        .build_op_with_inspector(inspector)
        .with_precompiles(OpPrecompiles::default())
}
