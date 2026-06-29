use super::abi::IArbAggregator;
use super::state::ArbStorage;
use super::util::{dispatch, empty_revert, finish_call};
use super::{ArbPrecompileInput, ArbitrumContext, BATCH_POSTER_ADDRESS};
use alloy::primitives::U256;
use revm::precompile::{PrecompileError, PrecompileResult};
use revm::Database;

pub(super) struct ArbAggregator;

const ARBOS_VERSION_SOLIDITY_REVERTS: u64 = 11;

impl ArbAggregator {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let caller = input.caller;
        let is_static = input.is_static;
        let current_arbos_version = input.current_arbos_version;
        let context = input.context;
        dispatch::<IArbAggregator::IArbAggregatorCalls>(data, gas_limit, |call, initial_gas| {
            let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
            match call {
                IArbAggregator::IArbAggregatorCalls::getPreferredAggregator(_) => {
                    finish_call::<IArbAggregator::getPreferredAggregatorCall>(
                        gas_limit,
                        storage.gas_used,
                        (BATCH_POSTER_ADDRESS, true).into(),
                    )
                }
                IArbAggregator::IArbAggregatorCalls::getDefaultAggregator(_) => {
                    finish_call::<IArbAggregator::getDefaultAggregatorCall>(
                        gas_limit,
                        storage.gas_used,
                        BATCH_POSTER_ADDRESS,
                    )
                }
                IArbAggregator::IArbAggregatorCalls::getBatchPosters(_) => {
                    let ret = storage.batch_posters()?;
                    finish_call::<IArbAggregator::getBatchPostersCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbAggregator::IArbAggregatorCalls::addBatchPoster(call) => {
                    if is_static {
                        return empty_revert(gas_limit, storage.gas_used);
                    }
                    match Self::add_batch_poster(&mut storage, caller, call.newBatchPoster) {
                        Ok(()) => finish_call::<IArbAggregator::addBatchPosterCall>(
                            gas_limit,
                            storage.gas_used,
                            ().into(),
                        ),
                        Err(PrecompileError::Other(_)) => Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        ),
                        Err(error) => Err(error),
                    }
                }
                IArbAggregator::IArbAggregatorCalls::getFeeCollector(call) => {
                    match storage.batch_poster_pay_to(call.batchPoster) {
                        Ok(ret) => finish_call::<IArbAggregator::getFeeCollectorCall>(
                            gas_limit,
                            storage.gas_used,
                            ret,
                        ),
                        Err(PrecompileError::Other(_)) => Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        ),
                        Err(error) => Err(error),
                    }
                }
                IArbAggregator::IArbAggregatorCalls::setFeeCollector(call) => {
                    if is_static {
                        return empty_revert(gas_limit, storage.gas_used);
                    }
                    match Self::set_fee_collector(
                        &mut storage,
                        caller,
                        call.batchPoster,
                        call.newFeeCollector,
                    ) {
                        Ok(()) => finish_call::<IArbAggregator::setFeeCollectorCall>(
                            gas_limit,
                            storage.gas_used,
                            ().into(),
                        ),
                        Err(PrecompileError::Other(_)) => Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        ),
                        Err(error) => Err(error),
                    }
                }
                IArbAggregator::IArbAggregatorCalls::getTxBaseFee(_) => {
                    finish_call::<IArbAggregator::getTxBaseFeeCall>(
                        gas_limit,
                        storage.gas_used,
                        U256::ZERO,
                    )
                }
                IArbAggregator::IArbAggregatorCalls::setTxBaseFee(_) => {
                    finish_call::<IArbAggregator::setTxBaseFeeCall>(
                        gas_limit,
                        storage.gas_used,
                        ().into(),
                    )
                }
            }
        })
    }

    fn non_solidity_error(
        gas_limit: u64,
        gas_used: u64,
        current_arbos_version: u64,
    ) -> PrecompileResult {
        if current_arbos_version < ARBOS_VERSION_SOLIDITY_REVERTS {
            empty_revert(gas_limit, gas_limit)
        } else {
            empty_revert(gas_limit, gas_used)
        }
    }

    fn add_batch_poster<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: alloy::primitives::Address,
        new_batch_poster: alloy::primitives::Address,
    ) -> Result<(), PrecompileError> {
        let owners_key = storage.chain_owner_key();
        if !storage.address_set_contains(&owners_key, caller)? {
            return Err(PrecompileError::other("must be called by chain owner"));
        }
        if !storage.batch_poster_exists(new_batch_poster)? {
            storage.add_batch_poster(new_batch_poster, new_batch_poster)?;
        }
        Ok(())
    }

    fn set_fee_collector<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: alloy::primitives::Address,
        batch_poster: alloy::primitives::Address,
        new_fee_collector: alloy::primitives::Address,
    ) -> Result<(), PrecompileError> {
        let old_fee_collector = storage.batch_poster_pay_to(batch_poster)?;
        if caller != batch_poster && caller != old_fee_collector {
            let owners_key = storage.chain_owner_key();
            if !storage.address_set_contains(&owners_key, caller)? {
                return Err(PrecompileError::other(
                    "only a batch poster (or its fee collector / chain owner) may change its fee collector",
                ));
            }
        }
        storage.set_batch_poster_pay_to(batch_poster, new_fee_collector)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BASE_PRECOMPILE_GAS, STORAGE_READ_GAS};
    use super::*;
    use crate::arbitrum::arbos_state;
    use crate::arbitrum::context::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::primitives::Address;
    use alloy::sol_types::SolCall;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::context::{ContextTr, JournalTr};
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::{Context, MainContext};

    const WORD_COPY_GAS: u64 = 3;

    fn run_call(
        data: &[u8],
        caller: Address,
        arbos_version: u64,
        gas_limit: u64,
    ) -> PrecompileResult {
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");

        ArbAggregator::run(ArbPrecompileInput {
            data,
            gas: gas_limit,
            caller,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: arbos_version,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
    }

    fn run_add_batch_poster_as_non_owner(arbos_version: u64, gas_limit: u64) -> PrecompileResult {
        let data = IArbAggregator::addBatchPosterCall {
            newBatchPoster: Address::with_last_byte(2),
        }
        .abi_encode();

        run_call(&data, Address::with_last_byte(1), arbos_version, gas_limit)
    }

    #[test]
    fn non_owner_add_batch_poster_reverts_empty_from_solidity_revert_version() {
        let output =
            run_add_batch_poster_as_non_owner(11, u64::MAX).expect("addBatchPoster should revert");

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(
            output.gas_used,
            BASE_PRECOMPILE_GAS + WORD_COPY_GAS + STORAGE_READ_GAS
        );
    }

    #[test]
    fn non_owner_add_batch_poster_consumes_all_gas_before_solidity_revert_version() {
        let gas_limit = 1_000_000;
        let output =
            run_add_batch_poster_as_non_owner(10, gas_limit).expect("addBatchPoster should revert");

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(output.gas_used, gas_limit);
    }
}
