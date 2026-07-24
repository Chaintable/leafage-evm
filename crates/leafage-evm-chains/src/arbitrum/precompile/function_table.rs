use super::abi::IArbFunctionTable;
use super::state::ArbStorage;
use super::util::{dispatch, empty_revert, finish_call};
use super::{ArbPrecompileInput, ArbitrumContext};
use alloy::primitives::U256;
use revm::precompile::PrecompileResult;
use revm::Database;

pub(super) struct ArbFunctionTable;

const ARBOS_VERSION_SOLIDITY_REVERTS: u64 = 11;

impl ArbFunctionTable {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let current_arbos_version = input.current_arbos_version;
        let context = input.context;
        dispatch::<IArbFunctionTable::IArbFunctionTableCalls>(
            data,
            gas_limit,
            |call, initial_gas| {
                let storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
                match call {
                    IArbFunctionTable::IArbFunctionTableCalls::upload(_) => {
                        finish_call::<IArbFunctionTable::uploadCall>(
                            gas_limit,
                            storage.gas_used,
                            ().into(),
                        )
                    }
                    IArbFunctionTable::IArbFunctionTableCalls::size(_) => {
                        finish_call::<IArbFunctionTable::sizeCall>(
                            gas_limit,
                            storage.gas_used,
                            U256::ZERO,
                        )
                    }
                    IArbFunctionTable::IArbFunctionTableCalls::get(_) => {
                        if current_arbos_version < ARBOS_VERSION_SOLIDITY_REVERTS {
                            empty_revert(gas_limit, gas_limit)
                        } else {
                            empty_revert(gas_limit, storage.gas_used)
                        }
                    }
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::BASE_PRECOMPILE_GAS;
    use super::*;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::primitives::{Address, U256};
    use alloy::sol_types::SolCall;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::precompile::PrecompileOutput;
    use revm::{Context, MainContext};

    const TWO_WORD_COPY_GAS: u64 = 6;

    fn run_get(arbos_version: u64, gas_limit: u64) -> PrecompileOutput {
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(ArbitrumExecutionContext::default());
        let data = IArbFunctionTable::getCall {
            addr: Address::ZERO,
            index: U256::ZERO,
        }
        .abi_encode();

        let output = ArbFunctionTable::run(ArbPrecompileInput {
            data: &data,
            gas: gas_limit,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: arbos_version,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("get should revert");

        output
    }

    #[test]
    fn get_reverts_empty_for_empty_table_from_solidity_revert_version() {
        let output = run_get(11, u64::MAX);

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(output.gas_used, BASE_PRECOMPILE_GAS + TWO_WORD_COPY_GAS);
    }

    #[test]
    fn get_consumes_all_gas_before_solidity_revert_version() {
        let gas_limit = 1_000_000;

        let output = run_get(10, gas_limit);

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(output.gas_used, gas_limit);
    }
}
