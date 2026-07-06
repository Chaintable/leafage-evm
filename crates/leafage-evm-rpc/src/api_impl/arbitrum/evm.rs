use leafage_evm_chains::arbitrum::evm::ArbitrumEvm;
use leafage_evm_chains::arbitrum::evm::ArbitrumExecutionContext;
use leafage_evm_chains::arbitrum::precompile::ArbitrumPrecompileEnv;
use leafage_evm_chains::arbitrum::ArbitrumHardfork;
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::database::{DatabaseRef, WrapDatabaseRef};

pub(crate) fn create_arbitrum_evm_from_state<StateDB, INSP>(
    block_env: BlockEnv,
    cfg: CfgEnv<ArbitrumHardfork>,
    state: StateDB,
    inspector: INSP,
    precompile_env: ArbitrumPrecompileEnv,
    execution_context: ArbitrumExecutionContext,
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
        execution_context,
    )
}
