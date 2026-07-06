use super::abi::IArbInfo;
use super::util::{copy_gas, dispatch, finish_call};
use super::{ArbPrecompileInput, ArbitrumContext};
use revm::context::{ContextTr, JournalTr};
use revm::precompile::{PrecompileError, PrecompileResult};
use revm::Database;

pub(super) struct ArbInfo;

const BALANCE_GAS: u64 = 700;
const CODE_STORAGE_READ_GAS: u64 = 2_100;

impl ArbInfo {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let context = input.context;
        dispatch::<IArbInfo::IArbInfoCalls>(data, gas_limit, |call, initial_gas| match call {
            IArbInfo::IArbInfoCalls::getBalance(call) => {
                let gas_used = initial_gas.saturating_add(BALANCE_GAS);
                if gas_used > gas_limit {
                    return Err(PrecompileError::OutOfGas);
                }
                let info = context
                    .journal_mut()
                    .load_account(call.account)
                    .map_err(|e| PrecompileError::other(format!("{e:?}")))?;
                finish_call::<IArbInfo::getBalanceCall>(gas_limit, gas_used, info.data.info.balance)
            }
            IArbInfo::IArbInfoCalls::getCode(call) => {
                let gas_used = initial_gas.saturating_add(CODE_STORAGE_READ_GAS);
                if gas_used > gas_limit {
                    return Err(PrecompileError::OutOfGas);
                }
                let loaded = context
                    .journal_mut()
                    .load_account_with_code(call.account)
                    .map_err(|e| PrecompileError::other(format!("{e:?}")))?;
                let code = loaded
                    .data
                    .info
                    .code
                    .as_ref()
                    .map(|code| code.original_bytes())
                    .unwrap_or_default();
                let gas_used = gas_used.saturating_add(copy_gas(code.len()));
                if gas_used > gas_limit {
                    return Err(PrecompileError::OutOfGas);
                }
                finish_call::<IArbInfo::getCodeCall>(gas_limit, gas_used, code)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::util::copy_gas;
    use super::super::BASE_PRECOMPILE_GAS;
    use super::*;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::primitives::{Address, Bytes, U256};
    use alloy::sol_types::SolCall;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::state::{AccountInfo, Bytecode};
    use revm::{Context, MainContext};

    const WORD_COPY_GAS: u64 = 3;

    fn context_with_account(
        account: Address,
        balance: U256,
        code: Bytes,
    ) -> ArbitrumContext<CacheDB<EmptyDB>> {
        let bytecode = Bytecode::new_legacy(code);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            account,
            AccountInfo {
                balance,
                code_hash: bytecode.hash_slow(),
                code: Some(bytecode),
                ..Default::default()
            },
        );
        Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(db)
            .with_chain(ArbitrumExecutionContext::default())
    }

    fn input<'a, DB: Database>(
        data: &'a [u8],
        gas: u64,
        context: &'a mut ArbitrumContext<DB>,
    ) -> ArbPrecompileInput<'a, ArbitrumContext<DB>> {
        ArbPrecompileInput {
            data,
            gas,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 0,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context,
        }
    }

    #[test]
    fn get_balance_charges_nitro_balance_gas() {
        let account = Address::from([0x11; 20]);
        let balance = U256::from(123);
        let data = IArbInfo::getBalanceCall { account }.abi_encode();
        let mut context = context_with_account(account, balance, Bytes::new());

        let output =
            ArbInfo::run(input(&data, 10_000, &mut context)).expect("getBalance should succeed");

        assert!(!output.reverted);
        assert_eq!(U256::from_be_slice(output.bytes.as_ref()), balance);
        assert_eq!(
            output.gas_used,
            BASE_PRECOMPILE_GAS + WORD_COPY_GAS + BALANCE_GAS + WORD_COPY_GAS
        );
    }

    #[test]
    fn get_code_charges_nitro_storage_read_and_raw_code_copy_gas() {
        let account = Address::from([0x22; 20]);
        let code = Bytes::from(vec![0xaa; 33]);
        let data = IArbInfo::getCodeCall { account }.abi_encode();
        let mut context = context_with_account(account, U256::ZERO, code.clone());
        let encoded = IArbInfo::getCodeCall::abi_encode_returns(&code);

        let output =
            ArbInfo::run(input(&data, 10_000, &mut context)).expect("getCode should succeed");

        assert!(!output.reverted);
        assert_eq!(
            IArbInfo::getCodeCall::abi_decode_returns(&output.bytes).unwrap(),
            code
        );
        assert_eq!(copy_gas(code.len()), 6);
        assert_eq!(copy_gas(encoded.len()), 12);
        assert_eq!(
            output.gas_used,
            BASE_PRECOMPILE_GAS + WORD_COPY_GAS + CODE_STORAGE_READ_GAS + 6 + 12
        );
    }

    #[test]
    fn get_code_oog_before_fixed_storage_read_cost() {
        let account = Address::from([0x33; 20]);
        let data = IArbInfo::getCodeCall { account }.abi_encode();
        let mut context = context_with_account(account, U256::ZERO, Bytes::from_static(&[0xaa]));
        let gas_limit = BASE_PRECOMPILE_GAS + WORD_COPY_GAS + CODE_STORAGE_READ_GAS - 1;

        let error = ArbInfo::run(input(&data, gas_limit, &mut context))
            .expect_err("getCode should run out of gas before loading code");

        assert!(error.is_oog());
    }
}
