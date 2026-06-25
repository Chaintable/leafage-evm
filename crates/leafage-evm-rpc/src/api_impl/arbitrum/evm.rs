use leafage_evm_chains::arbitrum::evm::ArbitrumEvm;
use leafage_evm_chains::arbitrum::precompile::ArbitrumPrecompileEnv;
use leafage_evm_chains::arbitrum::ArbitrumHardfork;
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::database::{DatabaseRef, WrapDatabaseRef};

pub(crate) fn create_arbitrum_evm_from_state_with_env<StateDB, INSP>(
    block_env: BlockEnv,
    cfg: CfgEnv<ArbitrumHardfork>,
    state: StateDB,
    inspector: INSP,
    precompile_env: ArbitrumPrecompileEnv,
) -> ArbitrumEvm<WrapDatabaseRef<StateDB>, INSP>
where
    StateDB: DatabaseRef,
{
    ArbitrumEvm::new(
        block_env,
        cfg,
        WrapDatabaseRef(state),
        inspector,
        precompile_env,
    )
}
