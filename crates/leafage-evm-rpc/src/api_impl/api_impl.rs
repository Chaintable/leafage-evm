use revm::primitives::{CfgEnv, SpecId};
use std::time::Duration;
/// [`ApiImpl`] implements the EthApi trait.
pub struct ApiImpl<DB> {
    pub db: DB,
    pub cfg: CfgEnv,
    pub spec_id: SpecId,
    pub time_out: Duration,
}

impl<DB> ApiImpl<DB> {
    pub fn new(db: DB, cfg: CfgEnv, spec_id: SpecId, time_out: Duration) -> Self {
        Self {
            db,
            cfg,
            spec_id,
            time_out,
        }
    }
}
