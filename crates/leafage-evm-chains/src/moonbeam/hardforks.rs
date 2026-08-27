//! Moonbeam/Moonriver hardfork newtype wrapper around [`MainnetSpecId`].
//!
//! Moonbeam and Moonriver are Frontier-based EVM chains that track the standard
//! Ethereum opcode/precompile set, so behaviorally this is identical to the
//! mainnet spec. The newtype exists only so that
//! `ApiImpl<DB, MoonbeamHardfork, _>` is a distinct type from
//! `ApiImpl<DB, MainnetSpecId, _>` for the purposes of trait dispatch — same
//! pattern as `IotexHardfork` and `CosmosHardfork`.
//!
//! The concrete spec is supplied by the CLI (`--spec-id`, defaulting to the
//! latest mainnet spec), which transitively makes revm's standard precompile
//! set cover the Ethereum-range Moonbeam precompiles (`0x01..=0x09`, the
//! BLS12-381 suite `0x0b..=0x11`, and P256 `0x100`). The Frontier-custom and
//! Substrate-backed precompiles are handled separately — see
//! [`crate::moonbeam::precompile`].

use leafage_evm_types::MainnetSpecId;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MoonbeamHardfork(MainnetSpecId);

impl Deref for MoonbeamHardfork {
    type Target = MainnetSpecId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for MoonbeamHardfork {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<MainnetSpecId> for MoonbeamHardfork {
    fn from(spec: MainnetSpecId) -> Self {
        Self(spec)
    }
}

impl From<MoonbeamHardfork> for MainnetSpecId {
    fn from(spec: MoonbeamHardfork) -> Self {
        spec.0
    }
}
