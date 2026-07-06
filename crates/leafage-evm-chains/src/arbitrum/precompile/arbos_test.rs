use super::abi::IArbosTest;
use super::state::ArbStorage;
use super::util::{dispatch, empty_revert, finish_call};
use super::{ArbPrecompileInput, ArbitrumContext};
use revm::precompile::PrecompileResult;
use revm::Database;

pub(super) struct ArbosTest;

const ARBOS_VERSION_SOLIDITY_REVERTS: u64 = 11;

impl ArbosTest {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let current_arbos_version = input.current_arbos_version;
        let context = input.context;
        dispatch::<IArbosTest::IArbosTestCalls>(data, gas_limit, |call, initial_gas| {
            let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
            match call {
                IArbosTest::IArbosTestCalls::burnArbGas(call) => {
                    let Ok(gas) = u64::try_from(call.gasAmount) else {
                        return if current_arbos_version < ARBOS_VERSION_SOLIDITY_REVERTS {
                            empty_revert(gas_limit, gas_limit)
                        } else {
                            empty_revert(gas_limit, storage.gas_used)
                        };
                    };
                    if storage.burn(gas).is_err() {
                        storage.burn_out();
                    }
                    finish_call::<IArbosTest::burnArbGasCall>(
                        gas_limit,
                        storage.gas_used,
                        ().into(),
                    )
                }
            }
        })
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
    use revm::{Context, MainContext};

    const WORD_COPY_GAS: u64 = 3;

    fn context() -> ArbitrumContext<CacheDB<EmptyDB>> {
        Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(ArbitrumExecutionContext::default())
    }

    fn run_burn(gas_amount: U256, gas_limit: u64) -> PrecompileResult {
        run_burn_with_version(gas_amount, gas_limit, ARBOS_VERSION_SOLIDITY_REVERTS)
    }

    fn run_burn_with_version(
        gas_amount: U256,
        gas_limit: u64,
        arbos_version: u64,
    ) -> PrecompileResult {
        let data = IArbosTest::burnArbGasCall {
            gasAmount: gas_amount,
        }
        .abi_encode();
        let mut context = context();
        ArbosTest::run(ArbPrecompileInput {
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
    }

    #[test]
    fn burn_arb_gas_charges_requested_amount() {
        let gas_to_burn = 17;
        let gas_limit = 1_000;
        let output = run_burn(U256::from(gas_to_burn), gas_limit).expect("burn should succeed");

        assert!(!output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(
            output.gas_used,
            BASE_PRECOMPILE_GAS + WORD_COPY_GAS + gas_to_burn
        );
    }

    #[test]
    fn burn_arb_gas_saturates_when_amount_exceeds_remaining_gas() {
        let gas_limit = BASE_PRECOMPILE_GAS + WORD_COPY_GAS + 1;
        let output = run_burn(U256::from(u64::MAX), gas_limit).expect("burn should succeed");

        assert!(!output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(output.gas_used, gas_limit);
    }

    #[test]
    fn burn_arb_gas_reverts_empty_when_amount_is_not_u64() {
        let output =
            run_burn(U256::from(u64::MAX) + U256::from(1), u64::MAX).expect("burn should revert");

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(output.gas_used, BASE_PRECOMPILE_GAS + WORD_COPY_GAS);
    }

    #[test]
    fn burn_arb_gas_consumes_all_gas_when_amount_is_not_u64_before_solidity_revert_version() {
        let gas_limit = 1_000_000;
        let output = run_burn_with_version(
            U256::from(u64::MAX) + U256::from(1),
            gas_limit,
            ARBOS_VERSION_SOLIDITY_REVERTS - 1,
        )
        .expect("burn should revert");

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(output.gas_used, gas_limit);
    }
}
