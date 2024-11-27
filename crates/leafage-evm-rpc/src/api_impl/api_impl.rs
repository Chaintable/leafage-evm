use revm::primitives::{CfgEnv, SpecId};
/// [`ApiImpl`] implements the EthApi trait.
pub struct ApiImpl<DB> {
    pub db: DB,
    pub cfg: CfgEnv,
    pub spec_id: SpecId,
}

impl<DB> ApiImpl<DB> {
    pub fn new(db: DB, cfg: CfgEnv, spec_id: SpecId) -> Self {
        Self { db, cfg, spec_id }
    }
}
