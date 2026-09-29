//! `staking/util/staking_error.hpp` plus the `AbiDecodeError` and
//! `MathError` domains that can surface through the same `Result`.
//!
//! A failing precompile call reverts with the error *message* as raw revert
//! data (not an ABI encoded `Error(string)`) and consumes all gas
//! (`check_call_monad_precompile`).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StakingError {
    InternalError,
    MethodNotSupported,
    InvalidInput,
    ValidatorExists,
    UnknownValidator,
    WithdrawalIdExists,
    UnknownWithdrawalId,
    WithdrawalNotReady,
    InsufficientStake,
    InvalidSecpPubkey,
    InvalidBlsPubkey,
    InvalidSecpSignature,
    InvalidBlsSignature,
    SecpSignatureVerificationFailed,
    BlsSignatureVerificationFailed,
    NotInValidatorSet,
    SolvencyError,
    RequiresAuthAddress,
    CommissionTooHigh,
    ValueNonZero,
    DelegationTooSmall,
    ExternalRewardTooSmall,
    ExternalRewardTooLarge,
    // AbiDecodeError
    InputTooShort,
    LengthMismatch,
    // MathError
    Overflow,
    Underflow,
    DivisionByZero,
    // MONAD_ASSERT_THROW sites; the node aborts the block instead.
    WithdrawalInsolvent,
    CompoundLogicError,
    InvalidListEntry,
}

impl StakingError {
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::InternalError => "internal error",
            Self::MethodNotSupported => "method not supported",
            Self::InvalidInput => "invalid input",
            Self::ValidatorExists => "validator exists",
            Self::UnknownValidator => "unknown validator",
            Self::WithdrawalIdExists => "withdrawal id exists",
            Self::UnknownWithdrawalId => "unknown withdrawal id",
            Self::WithdrawalNotReady => "withdrawal not ready",
            Self::InsufficientStake => "insufficient stake",
            Self::InvalidSecpPubkey => "invalid secp pubkey",
            Self::InvalidBlsPubkey => "invalid bls pubkey",
            Self::InvalidSecpSignature => "invalid secp signature",
            Self::InvalidBlsSignature => "invalid bls signature",
            Self::SecpSignatureVerificationFailed => "secp signature verification failed",
            Self::BlsSignatureVerificationFailed => "bls signature verification failed",
            Self::NotInValidatorSet => "not in validator set",
            Self::SolvencyError => "solvency error",
            Self::RequiresAuthAddress => "requires auth address",
            Self::CommissionTooHigh => "commission too high",
            Self::ValueNonZero => "value is nonzero",
            Self::DelegationTooSmall => "delegation is too small",
            Self::ExternalRewardTooSmall => "external reward too small",
            Self::ExternalRewardTooLarge => "external reward too large",
            Self::InputTooShort => "input too short",
            Self::LengthMismatch => "length mismatch",
            Self::Overflow => "overflow",
            Self::Underflow => "underflow",
            Self::DivisionByZero => "division by zero",
            Self::WithdrawalInsolvent => "withdrawal insolvent",
            Self::CompoundLogicError => "staking compound logic error",
            Self::InvalidListEntry => "invalid list entry",
        }
    }
}

/// Outcome of a precompile method: a revert carrying a message, or a fatal
/// database error that aborts execution.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Revert with the message as raw revert data.
    Revert(&'static str),
    /// Database error, aborts execution.
    Fatal(String),
}

impl From<StakingError> for Failure {
    fn from(error: StakingError) -> Self {
        Self::Revert(error.message())
    }
}

pub(crate) type StakingResult<T> = Result<T, Failure>;
