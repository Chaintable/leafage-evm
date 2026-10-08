//! Result types used internally by the Arbitrum precompiles.
//!
//! revm 37+ reduced `revm::precompile::PrecompileError` to fatal-only errors and moved
//! non-fatal failures into `PrecompileOutput::status`. The Arbitrum precompiles never go
//! through revm's `Precompile` interface — they are converted to an `InterpreterResult`
//! by [`super::util::to_interpreter_result`] — and rely on the out-of-gas / other / fatal
//! distinction (including `?` propagation) for control flow, so they keep these local
//! equivalents of the pre-37 types.

use alloy::primitives::Bytes;
use std::borrow::Cow;
use std::fmt;

/// Result of an Arbitrum precompile call.
pub(crate) type PrecompileResult = Result<PrecompileOutput, PrecompileError>;

/// Successful or reverted output of an Arbitrum precompile call.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PrecompileOutput {
    /// Gas used by the precompile.
    pub(crate) gas_used: u64,
    /// Output bytes.
    pub(crate) bytes: Bytes,
    /// Whether the precompile reverted.
    pub(crate) reverted: bool,
}

impl PrecompileOutput {
    /// Returns new precompile output with the given gas used and output bytes.
    pub(crate) fn new(gas_used: u64, bytes: Bytes) -> Self {
        Self {
            gas_used,
            bytes,
            reverted: false,
        }
    }

    /// Returns new precompile revert with the given gas used and output bytes.
    pub(crate) fn new_reverted(gas_used: u64, bytes: Bytes) -> Self {
        Self {
            gas_used,
            bytes,
            reverted: true,
        }
    }
}

/// Failure of an Arbitrum precompile call.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PrecompileError {
    /// The call ran out of gas; halts the frame with `PrecompileOOG`.
    OutOfGas,
    /// Non-fatal failure; halts the frame with `PrecompileError`.
    Other(Cow<'static, str>),
    /// Unrecoverable failure (e.g. a database error); aborts EVM execution.
    Fatal(String),
}

impl PrecompileError {
    /// Returns a non-fatal error with the given message.
    pub(crate) fn other(err: impl Into<String>) -> Self {
        Self::Other(Cow::Owned(err.into()))
    }

    /// Returns `true` if the error is out of gas.
    pub(crate) fn is_oog(&self) -> bool {
        matches!(self, Self::OutOfGas)
    }
}

impl fmt::Display for PrecompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfGas => f.write_str("out of gas"),
            Self::Other(s) => f.write_str(s),
            Self::Fatal(s) => f.write_str(s),
        }
    }
}

impl core::error::Error for PrecompileError {}
