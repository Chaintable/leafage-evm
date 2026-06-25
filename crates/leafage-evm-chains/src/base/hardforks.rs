use leafage_evm_types::{CfgEnv, OpSpecId};
use revm::primitives::hardfork::SpecId;
use std::ops::{Deref, DerefMut};

/// Base hardfork spec.
///
/// Base forked from the OP stack, so execution semantics are OP-equivalent and
/// this newtype just wraps [`OpSpecId`]. It exists as a distinct type so Base
/// gets its own `EvmExecutor` impl (and precompile provider) without colliding
/// with the plain `op` evm-type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BaseHardfork(OpSpecId);

impl Deref for BaseHardfork {
    type Target = OpSpecId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for BaseHardfork {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<OpSpecId> for BaseHardfork {
    fn from(spec: OpSpecId) -> Self {
        Self(spec)
    }
}

impl From<BaseHardfork> for OpSpecId {
    fn from(spec: BaseHardfork) -> Self {
        spec.0
    }
}

impl From<BaseHardfork> for SpecId {
    fn from(spec: BaseHardfork) -> Self {
        OpSpecId::from(spec).into()
    }
}

impl BaseHardfork {
    pub fn convert_cfg_env(cfg: CfgEnv<BaseHardfork>) -> CfgEnv<OpSpecId> {
        let spec: OpSpecId = cfg.spec.into();
        cfg.with_spec_and_mainnet_gas_params(spec)
    }
}
