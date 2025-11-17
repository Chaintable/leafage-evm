use leafage_evm_types::MainnetSpecId;
use std::ops::{Deref, DerefMut};

#[derive(Debug, Clone, Copy)]
pub struct CosmosHardfork(MainnetSpecId);

impl Deref for CosmosHardfork {
    type Target = MainnetSpecId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for CosmosHardfork {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<MainnetSpecId> for CosmosHardfork {
    fn from(spec: MainnetSpecId) -> Self {
        Self(spec)
    }
}

impl From<CosmosHardfork> for MainnetSpecId {
    fn from(spec: CosmosHardfork) -> Self {
        spec.0
    }
}
