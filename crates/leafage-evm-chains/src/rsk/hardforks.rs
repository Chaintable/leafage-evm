//! RSK hardfork newtype wrapper around [`MainnetSpecId`]. The newtype exists so
//! that `ApiImpl<DB, RskHardfork, _>` is a distinct type from
//! `ApiImpl<DB, MainnetSpecId, _>` for the purposes of trait dispatch — same
//! pattern as `IotexHardfork` and `CosmosHardfork`.
//!
//! RSK activates opcodes through RSKIPs rather than Ethereum hardforks. Its
//! instruction set matches Cancun minus the blob opcodes (PUSH0, MCOPY,
//! TLOAD/TSTORE, BASEFEE = the block minimum gas price), so Cancun is the spec
//! to run it with. Two things do NOT match and are out of reach of a spec id:
//!
//! * the gas schedule — RSK never adopted EIP-2929 access lists (SLOAD is 200,
//!   CALL 700, ...), so gas used / estimations differ from a real node;
//! * `DIFFICULTY` (0x44) — RSK returns the block difficulty, revm returns
//!   PREVRANDAO from Paris on.

use leafage_evm_types::MainnetSpecId;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RskHardfork(MainnetSpecId);

impl Deref for RskHardfork {
    type Target = MainnetSpecId;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for RskHardfork {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<MainnetSpecId> for RskHardfork {
    fn from(spec: MainnetSpecId) -> Self {
        Self(spec)
    }
}

impl From<RskHardfork> for MainnetSpecId {
    fn from(spec: RskHardfork) -> Self {
        spec.0
    }
}
