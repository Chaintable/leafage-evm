//! IoTeX hardfork newtype wrapper around [`MainnetSpecId`]. IoTeX runs the
//! standard ETH hardfork sequence, so behaviorally this is identical to the
//! mainnet spec. The newtype exists only so that
//! `ApiImpl<DB, IotexHardfork, _>` is a distinct type from
//! `ApiImpl<DB, MainnetSpecId, _>` for the purposes of trait dispatch — same
//! pattern as `CosmosHardfork`.

use leafage_evm_types::MainnetSpecId;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IotexHardfork(MainnetSpecId);

impl Deref for IotexHardfork {
    type Target = MainnetSpecId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for IotexHardfork {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<MainnetSpecId> for IotexHardfork {
    fn from(spec: MainnetSpecId) -> Self {
        Self(spec)
    }
}

impl From<IotexHardfork> for MainnetSpecId {
    fn from(spec: IotexHardfork) -> Self {
        spec.0
    }
}
