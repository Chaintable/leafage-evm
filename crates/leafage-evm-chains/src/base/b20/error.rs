//! Error type for the B20 precompile port.
//!
//! Mirrors Base reth's `BasePrecompileError` (`base/crates/common/precompile-storage/src/error.rs`)
//! with the variants leafage can actually produce. The distinction that matters for gas is
//! `Revert` (consumes the gas metered up to the revert point, returns ABI-encoded data) vs
//! `OutOfGas` (consumes the whole call gas limit).

use alloy::primitives::Bytes;
use alloy::sol_types::SolError;

/// Result alias for B20 operations.
pub type Result<T> = core::result::Result<T, B20Error>;

/// Failure modes of a B20 precompile call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum B20Error {
    /// Revert with ABI-encoded error data. Gas metered so far is consumed.
    Revert(Bytes),
    /// The call ran out of gas. Consumes the entire gas limit.
    OutOfGas,
    /// A state-mutating operation was attempted inside a static call.
    StaticCallViolation,
    /// A Solidity panic carrying its code: `0x11` arithmetic under/overflow, `0x21` enum
    /// conversion. Base classifies panics as system errors, so they propagate through an
    /// announcement's internal calls instead of being wrapped.
    Panic(u8),
    /// Unrecoverable storage/database failure. Not a revert: it aborts execution.
    Fatal(String),
}

/// Solidity `Panic(uint256)` selector.
const PANIC_SELECTOR: [u8; 4] = [0x4e, 0x48, 0x7b, 0x71];
/// Solidity panic code for arithmetic under/overflow.
const PANIC_UNDER_OVERFLOW: u8 = 0x11;
/// Solidity panic code for an out-of-range enum conversion.
const PANIC_ENUM_CONVERSION: u8 = 0x21;

impl B20Error {
    /// Builds a revert carrying `err` ABI-encoded.
    pub fn revert<E: SolError>(err: E) -> Self {
        Self::Revert(err.abi_encode().into())
    }

    /// Builds an empty revert (no return data).
    pub fn empty_revert() -> Self {
        Self::Revert(Bytes::new())
    }

    /// Builds the arithmetic under/overflow panic.
    pub fn under_overflow() -> Self {
        Self::Panic(PANIC_UNDER_OVERFLOW)
    }

    /// Builds the enum-conversion panic.
    pub fn enum_conversion() -> Self {
        Self::Panic(PANIC_ENUM_CONVERSION)
    }

    /// Returns the ABI-encoded revert payload for the revert-shaped variants.
    ///
    /// `Panic(code)` encodes as Solidity's `Panic(uint256)`, matching what the EVM produces
    /// for the equivalent Solidity failure.
    pub fn revert_output(&self) -> Option<Bytes> {
        match self {
            Self::Revert(data) => Some(data.clone()),
            Self::Panic(code) => {
                let mut out = Vec::with_capacity(36);
                out.extend_from_slice(&PANIC_SELECTOR);
                out.extend_from_slice(&[0u8; 31]);
                out.push(*code);
                Some(out.into())
            }
            Self::OutOfGas | Self::StaticCallViolation | Self::Fatal(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_overflow_encodes_solidity_panic_0x11() {
        let out = B20Error::under_overflow().revert_output().unwrap();
        assert_eq!(out.len(), 36);
        assert_eq!(&out[..4], &PANIC_SELECTOR);
        assert_eq!(out[35], 0x11);
        assert!(out[4..35].iter().all(|b| *b == 0));
    }

    #[test]
    fn enum_conversion_encodes_solidity_panic_0x21() {
        let out = B20Error::enum_conversion().revert_output().unwrap();
        assert_eq!(out[35], 0x21);
    }

    #[test]
    fn non_revert_variants_have_no_output() {
        assert!(B20Error::OutOfGas.revert_output().is_none());
        assert!(B20Error::StaticCallViolation.revert_output().is_none());
        assert!(B20Error::Fatal("db".into()).revert_output().is_none());
    }
}
