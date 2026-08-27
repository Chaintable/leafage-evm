//! The storage/gas port the B20 logic runs against.
//!
//! Mirrors Base reth's `PrecompileStorageProvider`
//! (`base/crates/common/precompile-storage/src/provider.rs`), trimmed to the surface leafage's
//! read-only node needs. All gas accounting lives *behind* this trait, in the implementor:
//! the token logic below never names a gas constant, exactly as in Base. That is what makes
//! the port gas-exact — costs come from the same EIP-2929/2200/3529 rules the EVM applies,
//! driven by the journal's own cold/warm and original/present/new values.
//!
//! Note that mapping-slot derivation is *not* metered: Base charges keccak gas only in the
//! B20 factory's address derivation (`b20_factory/dispatch.rs`), never for
//! `keccak256(key ++ slot)` in a mapping access. Slot math here is therefore free, and only
//! the resulting `sload`/`sstore` is charged.

use alloy::primitives::{Address, LogData, U256};

use super::error::Result;

/// Storage, event, and gas access for a B20 precompile call.
///
/// Implementors charge gas inside each method; a caller that completes without an
/// `OutOfGas` error has, by construction, paid the same gas an equivalent Solidity
/// contract would have.
pub trait B20Port {
    /// Reads storage at `address`, charging EIP-2929 warm/cold read cost.
    fn sload(&mut self, address: Address, key: U256) -> Result<U256>;

    /// Writes storage at `address`, charging EIP-2929/2200 cost and recording the
    /// EIP-3529 refund. Errors with `StaticCallViolation` in a static context and with
    /// `OutOfGas` when remaining gas is at or below the 2300 call stipend.
    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()>;

    /// Emits a log from `address`, charging `LOG + 375/topic + 8/byte`.
    fn emit_event(&mut self, address: Address, log: LogData) -> Result<()>;

    /// Returns whether `address` has non-empty code, charging EIP-2929 account access cost.
    fn has_code(&mut self, address: Address) -> Result<bool>;

    /// Charges `gas` directly (used for the per-word calldata charge).
    fn deduct_gas(&mut self, gas: u64) -> Result<()>;

    /// The immediate caller of the precompile.
    fn caller(&self) -> Address;

    /// Wei attached to the call. B20 selectors are all nonpayable.
    fn call_value(&self) -> U256;

    /// Chain ID, for the EIP-712 domain separator.
    fn chain_id(&self) -> u64;

    /// Block timestamp, for permit deadline checks.
    fn timestamp(&self) -> U256;

    /// Whether the call frame forbids state mutation.
    fn is_static(&self) -> bool;
}
