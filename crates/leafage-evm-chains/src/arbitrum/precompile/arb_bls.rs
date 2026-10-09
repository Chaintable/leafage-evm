use super::util::decode_revert;
use super::{ArbPrecompileInput, ArbitrumContext};
use crate::arbitrum::precompile::result::PrecompileResult;
use revm::Database;

pub(super) struct ArbBls;

impl ArbBls {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        decode_revert(input.gas, "unknown Arbitrum precompile selector")
    }
}
