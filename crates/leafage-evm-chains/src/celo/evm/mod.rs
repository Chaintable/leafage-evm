mod precompile;

use super::CeloConfig;
use leafage_evm_types::{BlockEnv, CfgEnv, OpSpecId};
use op_revm::{DefaultOp, OpBuilder, OpContext, OpEvm};
use revm::{
    handler::instructions::EthInstructions, interpreter::interpreter::EthInterpreter, Context,
    Database,
};

pub use precompile::CeloPrecompiles;

pub type CeloEvm<DB, INSP> =
    OpEvm<OpContext<DB>, INSP, EthInstructions<EthInterpreter, OpContext<DB>>, CeloPrecompiles>;

pub fn create_celo_evm<DB: Database, INSP>(
    block: BlockEnv,
    cfg: CfgEnv<OpSpecId>,
    db: DB,
    inspector: INSP,
) -> Result<CeloEvm<DB, INSP>, String> {
    let cfg = CeloConfig::cfg_at(cfg, &block)?;
    let precompiles = CeloPrecompiles::new(cfg.spec);
    Ok(Context::op()
        .with_block(block)
        .with_cfg(cfg)
        .with_db(db)
        .build_op_with_inspector(inspector)
        .with_precompiles(precompiles))
}
