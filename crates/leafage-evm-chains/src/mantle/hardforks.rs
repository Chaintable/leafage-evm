use leafage_evm_types::{CfgEnv, OpSpecId};
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MantleHardfork(OpSpecId);

impl Deref for MantleHardfork {
    type Target = OpSpecId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for MantleHardfork {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<OpSpecId> for MantleHardfork {
    fn from(spec: OpSpecId) -> Self {
        Self(spec)
    }
}

impl From<MantleHardfork> for OpSpecId {
    fn from(spec: MantleHardfork) -> Self {
        spec.0
    }
}

impl MantleHardfork {
    pub fn convert_cfg_env(cfg: CfgEnv<MantleHardfork>) -> CfgEnv<OpSpecId> {
        let spec = cfg.spec.into();
        cfg.with_spec(spec)
    }
}
