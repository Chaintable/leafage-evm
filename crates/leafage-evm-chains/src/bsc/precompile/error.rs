use revm::precompile::PrecompileError;
use std::borrow::Cow;

/// BSC specific precompile errors.
#[derive(Debug, PartialEq)]
pub enum BscPrecompileError {
    /// The cometbft validation input is invalid.
    InvalidInput,
    /// The cometbft apply block failed.
    CometBftApplyBlockFailed,
    /// The cometbft consensus state encoding failed.
    CometBftEncodeConsensusStateFailed,
    /// The double sign invalid evidence.
    DoubleSignInvalidEvidence,
}

impl From<BscPrecompileError> for PrecompileError {
    fn from(error: BscPrecompileError) -> Self {
        match error {
            BscPrecompileError::InvalidInput => {
                PrecompileError::Other(Cow::Borrowed("invalid input"))
            }
            BscPrecompileError::CometBftApplyBlockFailed => {
                PrecompileError::Other(Cow::Borrowed("apply block failed"))
            }
            BscPrecompileError::CometBftEncodeConsensusStateFailed => {
                PrecompileError::Other(Cow::Borrowed("encode consensus state failed"))
            }
            BscPrecompileError::DoubleSignInvalidEvidence => {
                PrecompileError::Other(Cow::Borrowed("double sign invalid evidence"))
            }
        }
    }
}
