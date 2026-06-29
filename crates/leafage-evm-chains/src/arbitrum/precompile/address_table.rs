use super::abi::IArbAddressTable;
use super::state::ArbStorage;
use super::util::{dispatch, empty_revert, finish_call};
use super::{ArbPrecompileInput, ArbitrumContext};
use alloy::primitives::{Bytes, U256};
use revm::precompile::PrecompileResult;
use revm::Database;

pub(super) struct ArbAddressTable;

const ARBOS_VERSION_SOLIDITY_REVERTS: u64 = 11;

impl ArbAddressTable {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let is_static = input.is_static;
        let current_arbos_version = input.current_arbos_version;
        let context = input.context;
        dispatch::<IArbAddressTable::IArbAddressTableCalls>(data, gas_limit, |call, initial_gas| {
            let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
            match call {
                IArbAddressTable::IArbAddressTableCalls::addressExists(call) => {
                    let exists = storage.address_table_lookup(call.addr)?.is_some();
                    finish_call::<IArbAddressTable::addressExistsCall>(
                        gas_limit,
                        storage.gas_used,
                        exists,
                    )
                }
                IArbAddressTable::IArbAddressTableCalls::lookup(call) => {
                    let Some(index) = storage.address_table_lookup(call.addr)? else {
                        return Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        );
                    };
                    finish_call::<IArbAddressTable::lookupCall>(
                        gas_limit,
                        storage.gas_used,
                        U256::from(index),
                    )
                }
                IArbAddressTable::IArbAddressTableCalls::lookupIndex(call) => {
                    let Ok(index) = u64::try_from(call.index) else {
                        return Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        );
                    };
                    let Some(addr) = storage.address_table_lookup_index(index)? else {
                        return Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        );
                    };
                    finish_call::<IArbAddressTable::lookupIndexCall>(
                        gas_limit,
                        storage.gas_used,
                        addr,
                    )
                }
                IArbAddressTable::IArbAddressTableCalls::size(_) => {
                    let size = storage.address_table_size()?;
                    finish_call::<IArbAddressTable::sizeCall>(gas_limit, storage.gas_used, size)
                }
                IArbAddressTable::IArbAddressTableCalls::compress(call) => {
                    let bytes = storage.address_table_compress(call.addr)?;
                    finish_call::<IArbAddressTable::compressCall>(
                        gas_limit,
                        storage.gas_used,
                        Bytes::from(bytes),
                    )
                }
                IArbAddressTable::IArbAddressTableCalls::decompress(call) => {
                    let Ok(offset) = usize::try_from(call.offset) else {
                        return Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        );
                    };
                    let Some(buf) = call.buf.get(offset..) else {
                        return Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        );
                    };
                    let Ok((addr, consumed)) = storage.address_table_decompress(buf) else {
                        return Self::non_solidity_error(
                            gas_limit,
                            storage.gas_used,
                            current_arbos_version,
                        );
                    };
                    finish_call::<IArbAddressTable::decompressCall>(
                        gas_limit,
                        storage.gas_used,
                        (addr, U256::from(consumed)).into(),
                    )
                }
                IArbAddressTable::IArbAddressTableCalls::register(call) => {
                    if is_static {
                        return empty_revert(gas_limit, storage.gas_used);
                    }
                    let slot = storage.address_table_register(call.addr)?;
                    finish_call::<IArbAddressTable::registerCall>(
                        gas_limit,
                        storage.gas_used,
                        U256::from(slot),
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

    fn run_call(data: &[u8], arbos_version: u64, gas_limit: u64) -> PrecompileResult {
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

        ArbAddressTable::run(ArbPrecompileInput {
            data,
            gas: gas_limit,
            caller: Address::ZERO,
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

    fn run_lookup_missing(arbos_version: u64, gas_limit: u64) -> PrecompileResult {
        let data = IArbAddressTable::lookupCall {
            addr: Address::with_last_byte(1),
        }
        .abi_encode();

        run_call(&data, arbos_version, gas_limit)
    }

    #[test]
    fn lookup_missing_address_reverts_empty_from_solidity_revert_version() {
        let output = run_lookup_missing(11, u64::MAX).expect("lookup should revert");

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(
            output.gas_used,
            BASE_PRECOMPILE_GAS + WORD_COPY_GAS + STORAGE_READ_GAS
        );
    }

    #[test]
    fn lookup_missing_address_consumes_all_gas_before_solidity_revert_version() {
        let gas_limit = 1_000_000;

        let output = run_lookup_missing(10, gas_limit).expect("lookup should revert");

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(output.gas_used, gas_limit);
    }

    #[test]
    fn lookup_index_invalid_uint_consumes_all_gas_before_solidity_revert_version() {
        let gas_limit = 1_000_000;
        let data = IArbAddressTable::lookupIndexCall {
            index: U256::from(u64::MAX) + U256::from(1),
        }
        .abi_encode();

        let output = run_call(&data, 10, gas_limit).expect("lookupIndex should revert");

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(output.gas_used, gas_limit);
    }

    #[test]
    fn decompress_invalid_offset_consumes_all_gas_before_solidity_revert_version() {
        let gas_limit = 1_000_000;
        let data = IArbAddressTable::decompressCall {
            buf: Bytes::from(vec![0]),
            offset: U256::from(2),
        }
        .abi_encode();

        let output = run_call(&data, 10, gas_limit).expect("decompress should revert");

        assert!(output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(output.gas_used, gas_limit);
    }
}
