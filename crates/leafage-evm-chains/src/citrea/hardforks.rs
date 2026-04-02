use leafage_evm_types::MainnetSpecId;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CitreaHardfork(MainnetSpecId);

impl Deref for CitreaHardfork {
    type Target = MainnetSpecId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for CitreaHardfork {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<MainnetSpecId> for CitreaHardfork {
    fn from(spec: MainnetSpecId) -> Self {
        Self(spec)
    }
}

impl From<CitreaHardfork> for MainnetSpecId {
    fn from(spec: CitreaHardfork) -> Self {
        spec.0
    }
}
