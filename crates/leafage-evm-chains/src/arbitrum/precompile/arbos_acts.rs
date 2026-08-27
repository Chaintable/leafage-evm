use super::abi::IArbosActs;
use super::util::{dispatch, sol_error_revert};
use super::{ArbPrecompileInput, ArbitrumContext};
use revm::precompile::PrecompileResult;
use revm::Database;

pub(super) struct ArbosActs;

impl ArbosActs {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        dispatch::<IArbosActs::IArbosActsCalls>(input.data, input.gas, |call, initial_gas| {
            match call {
                IArbosActs::IArbosActsCalls::startBlock(_)
                | IArbosActs::IArbosActsCalls::batchPostingReport(_)
                | IArbosActs::IArbosActsCalls::batchPostingReportV2(_) => {
                    sol_error_revert(input.gas, initial_gas, IArbosActs::CallerNotArbOS {})
                }
            }
        })
    }
}
