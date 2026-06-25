use super::abi::IArbStatistics;
use super::state::ArbStorage;
use super::util::{dispatch, finish_call};
use super::{ArbPrecompileInput, ArbitrumContext};
use alloy::primitives::U256;
use revm::context::ContextTr;
use revm::context_interface::Block;
use revm::precompile::PrecompileResult;
use revm::Database;

pub(super) struct ArbStatistics;

impl ArbStatistics {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let context = input.context;
        dispatch::<IArbStatistics::IArbStatisticsCalls>(data, gas_limit, |call, initial_gas| {
            let storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
            match call {
                IArbStatistics::IArbStatisticsCalls::getStats(_) => {
                    finish_call::<IArbStatistics::getStatsCall>(
                        gas_limit,
                        storage.gas_used,
                        (
                            storage.context.block().number(),
                            U256::ZERO,
                            U256::ZERO,
                            U256::ZERO,
                            U256::ZERO,
                            U256::ZERO,
                        )
                            .into(),
                    )
                }
            }
        })
    }
}
