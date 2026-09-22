//! RSK hardfork newtype wrapper around [`MainnetSpecId`]. The newtype exists so
//! that `ApiImpl<DB, RskHardfork, _>` is a distinct type from
//! `ApiImpl<DB, MainnetSpecId, _>` for the purposes of trait dispatch — same
//! pattern as `IotexHardfork` and `CosmosHardfork`.
//!
//! RSK activates opcodes through RSKIPs rather than Ethereum hardforks. Its
//! instruction set matches Cancun minus the blob opcodes (PUSH0, MCOPY,
//! TLOAD/TSTORE, BASEFEE = the block minimum gas price), so Cancun is the spec
//! to run it with. What a spec id cannot express is layered on top by
//! [`crate::rsk::RskEvm`]:
//!
//! * the gas schedule, frozen around EIP-150 (`rsk/gas.rs`);
//! * the opcodes that behave differently — the call family, `SSTORE`,
//!   `SELFDESTRUCT`, `EXTCODESIZE` / `EXTCODEHASH`, `DIFFICULTY`
//!   (`rsk/instructions.rs`);
//! * the 400 frame call depth limit, the refund cap and MODEXP pricing.

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
