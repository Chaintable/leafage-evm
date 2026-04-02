use leafage_evm_chains::citrea::{CitreaHardfork, CitreaPrecompiles};
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId};
use revm::context::{Evm, TxEnv};
use revm::database::{DatabaseRef, WrapDatabaseRef};
use revm::handler::{instructions::EthInstructions, EthPrecompiles};
use revm::interpreter::interpreter::EthInterpreter;
use revm::{Context, MainBuilder, MainContext};

pub(crate) fn create_citrea_evm_from_state<StateDB, INSP>(
    block_env: BlockEnv,
    cfg: CfgEnv<CitreaHardfork>,
    state: StateDB,
    inspector: INSP,
) -> Evm<
    Context<BlockEnv, TxEnv, CfgEnv<MainnetSpecId>, WrapDatabaseRef<StateDB>>,
    INSP,
    EthInstructions<
        EthInterpreter,
        Context<BlockEnv, TxEnv, CfgEnv<MainnetSpecId>, WrapDatabaseRef<StateDB>>,
    >,
    EthPrecompiles,
    revm::handler::EthFrame,
>
where
    StateDB: DatabaseRef,
{
    let spec = cfg.spec;
    let eth_precompiles: EthPrecompiles = CitreaPrecompiles::new(spec).into();
    let mainnet_cfg = cfg.with_spec_and_mainnet_gas_params(MainnetSpecId::from(spec));
    Context::mainnet()
        .with_block(block_env)
        .with_cfg(mainnet_cfg)
        .with_ref_db(state)
        .build_mainnet_with_inspector(inspector)
        .with_precompiles(eth_precompiles)
}
