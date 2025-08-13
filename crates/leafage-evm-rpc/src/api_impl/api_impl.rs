use leafage_evm_types::{CfgEnv, SpecId};
use std::time::Duration;
/// [`ApiImpl`] implements the EthApi trait.
pub struct ApiImpl<DB> {
    pub db: DB,
    pub cfg: CfgEnv<SpecId>,
    pub time_out: Duration,
    pub using_ovm: bool,
}

impl<DB> ApiImpl<DB> {
    pub fn new(db: DB, cfg: CfgEnv<SpecId>, time_out: Duration, using_ovm: bool) -> Self {
        Self {
            db,
            cfg,
            time_out,
            using_ovm,
        }
    }
}
