use leafage_evm_types::{CfgEnv, OpSpecId};
use revm::primitives::hardfork::SpecId;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HemiHardfork(OpSpecId);

impl Deref for HemiHardfork {
    type Target = OpSpecId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for HemiHardfork {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<OpSpecId> for HemiHardfork {
    fn from(spec: OpSpecId) -> Self {
        Self(spec)
    }
}

impl From<HemiHardfork> for OpSpecId {
    fn from(spec: HemiHardfork) -> Self {
        spec.0
    }
}

impl From<HemiHardfork> for SpecId {
    fn from(spec: HemiHardfork) -> Self {
        OpSpecId::from(spec).into()
    }
}

impl HemiHardfork {
    pub fn convert_cfg_env(cfg: CfgEnv<HemiHardfork>) -> CfgEnv<OpSpecId> {
        let spec: OpSpecId = cfg.spec.into();
        cfg.with_spec_and_mainnet_gas_params(spec)
    }
}
