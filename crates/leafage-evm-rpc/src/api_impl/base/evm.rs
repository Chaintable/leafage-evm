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
    // Base execution is OP-equivalent. The Beryl precompiles are added on top of
    // the op precompile set in a later stage; Stage 1 uses the op set as-is
    // (base ≡ op for execution), establishing the wiring.
    Context::op()
        .with_block(block_env)
        .with_cfg(BaseHardfork::convert_cfg_env(cfg))
        .with_ref_db(state)
        .build_op_with_inspector(inspector)
        .with_precompiles(OpPrecompiles::default())
}
