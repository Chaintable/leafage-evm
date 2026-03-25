//! Unified error handling for Tempo precompiles (leafage-evm adaptation).
//!
//! Provides [`TempoPrecompileError`] -- a simplified error enum with Fatal/Revert variants,
//! plus the [`IntoPrecompileResult`] trait for converting into revm's `PrecompileResult`.
//!
//! Unlike the original Tempo node which has per-precompile typed error variants (and an
//! ABI-selector decoder registry), leafage-evm is a read-only node and only needs the
//! essential error plumbing for storage operations.

use alloy::primitives::Bytes;
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};

/// Top-level error type for Tempo precompile operations in leafage-evm.
///
/// Simplified from the full Tempo enum (which has per-precompile typed variants).
/// We only need: Fatal (irrecoverable), Revert (ABI-encoded revert data), and OutOfGas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TempoPrecompileError {
    /// Gas limit exceeded during precompile execution.
    OutOfGas,

    /// The calldata's 4-byte selector does not match any known precompile function.
    UnknownFunctionSelector([u8; 4]),

    /// ABI-encoded revert data from a precompile business-logic error.
    /// In the full Tempo node these are typed per-precompile error enums;
    /// here we carry the raw encoded bytes.
    Revert(Bytes),

    /// Unrecoverable internal error (e.g. database failure).
    Fatal(String),
}

impl std::fmt::Display for TempoPrecompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfGas => write!(f, "Gas limit exceeded"),
            Self::UnknownFunctionSelector(sel) => {
                write!(f, "Unknown function selector: {sel:?}")
            }
            Self::Revert(data) => write!(f, "Revert({} bytes)", data.len()),
            Self::Fatal(msg) => write!(f, "Fatal precompile error: {msg}"),
        }
    }
}

impl std::error::Error for TempoPrecompileError {}

/// Result type alias for Tempo precompile operations.
pub type Result<T> = std::result::Result<T, TempoPrecompileError>;

impl TempoPrecompileError {
    /// Returns true if this error represents a system-level failure that must be propagated
    /// rather than swallowed, because state may be inconsistent.
    pub fn is_system_error(&self) -> bool {
        matches!(self, Self::OutOfGas | Self::Fatal(_))
    }

    /// Creates an arithmetic under/overflow panic error (Panic(0x11)).
    pub fn under_overflow() -> Self {
        // Panic selector (0x4e487b71) + uint256(0x11) = under/overflow
        Self::Revert(Bytes::from_static(&[
            0x4e, 0x48, 0x7b, 0x71, // Panic selector
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x11,
        ]))
    }

    /// ABI-encodes this error and wraps it as a reverted [`PrecompileResult`].
    ///
    /// # Errors
    /// - `PrecompileError::OutOfGas` -- if the variant is [`OutOfGas`](Self::OutOfGas)
    /// - `PrecompileError::Fatal` -- if the variant is [`Fatal`](Self::Fatal)
    pub fn into_precompile_result(self, gas_used: u64) -> PrecompileResult {
        match self {
            Self::OutOfGas => Err(PrecompileError::OutOfGas),
            Self::Fatal(msg) => Err(PrecompileError::Fatal(msg)),
            Self::UnknownFunctionSelector(selector) => {
                // Encode as a simple 4-byte revert
                Ok(PrecompileOutput::new_reverted(
                    gas_used,
                    Bytes::copy_from_slice(&selector),
                ))
            }
            Self::Revert(data) => Ok(PrecompileOutput::new_reverted(gas_used, data)),
        }
    }
}

impl From<alloy_evm::EvmInternalsError> for TempoPrecompileError {
    fn from(value: alloy_evm::EvmInternalsError) -> Self {
        Self::Fatal(value.to_string())
    }
}

/// Extension trait to convert `Result<T, TempoPrecompileError>` into a [`PrecompileResult`].
pub trait IntoPrecompileResult<T> {
    /// Converts `self` into a [`PrecompileResult`], using `encode_ok` for the success path.
    fn into_precompile_result(
        self,
        gas_used: u64,
        encode_ok: impl FnOnce(T) -> Bytes,
    ) -> PrecompileResult;
}

impl<T> IntoPrecompileResult<T> for Result<T> {
    fn into_precompile_result(
        self,
        gas_used: u64,
        encode_ok: impl FnOnce(T) -> Bytes,
    ) -> PrecompileResult {
        match self {
            Ok(res) => Ok(PrecompileOutput::new(gas_used, encode_ok(res))),
            Err(err) => err.into_precompile_result(gas_used),
        }
    }
}

impl<T> IntoPrecompileResult<T> for TempoPrecompileError {
    fn into_precompile_result(
        self,
        gas_used: u64,
        _encode_ok: impl FnOnce(T) -> Bytes,
    ) -> PrecompileResult {
        TempoPrecompileError::into_precompile_result(self, gas_used)
    }
}
