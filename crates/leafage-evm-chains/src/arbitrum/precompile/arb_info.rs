use super::abi::IArbInfo;
use super::util::{copy_gas, dispatch, finish_call};
use super::{ArbPrecompileInput, ArbitrumContext};
use crate::arbitrum::evm::ArbResourceKind;
use alloy::primitives::{Address, B256, Bytes, U256};
use revm::Database;
use revm::context::ContextTr;
use revm::precompile::{PrecompileError, PrecompileResult};
use revm::primitives::KECCAK_EMPTY;

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
                let balance = Self::account_balance_without_warming(context, call.account)
                    .map_err(|e| PrecompileError::Fatal(format!("{e:?}")))?;
                finish_call::<IArbInfo::getBalanceCall>(gas_limit, gas_used, balance)
            }
            IArbInfo::IArbInfoCalls::getCode(call) => {
                let gas_used = initial_gas.saturating_add(CODE_STORAGE_READ_GAS);
                if gas_used > gas_limit {
                    return Err(PrecompileError::OutOfGas);
                }
                context.chain_mut().record_multi_gas(
                    ArbResourceKind::StorageAccessRead,
                    CODE_STORAGE_READ_GAS,
                );
                let code = Self::account_code_without_warming(context, call.account)
                    .map_err(|e| PrecompileError::Fatal(format!("{e:?}")))?;
                let code_copy_gas = copy_gas(code.len());
                let gas_used = gas_used.saturating_add(code_copy_gas);
                if gas_used > gas_limit {
                    return Err(PrecompileError::OutOfGas);
                }
                context
                    .chain_mut()
                    .record_multi_gas(ArbResourceKind::StorageAccessRead, code_copy_gas);
                finish_call::<IArbInfo::getCodeCall>(gas_limit, gas_used, code)
            }
        })
    }

    fn account_balance_without_warming<DB: Database>(
        context: &mut ArbitrumContext<DB>,
        account: Address,
    ) -> Result<U256, DB::Error> {
        match context.journal().state.get(&account) {
            Some(account) if account.is_selfdestructed() => Ok(U256::ZERO),
            Some(account) => Ok(account.info.balance),
            None => context
                .db_mut()
                .basic(account)
                .map(|info| info.map(|info| info.balance).unwrap_or_default()),
        }
    }

    fn account_code_without_warming<DB: Database>(
        context: &mut ArbitrumContext<DB>,
        account: Address,
    ) -> Result<Bytes, DB::Error> {
        let source = match context.journal().state.get(&account) {
            Some(account) if account.is_selfdestructed() => AccountCodeSource::Ready(Bytes::new()),
            Some(account) => match &account.info.code {
                Some(code) => AccountCodeSource::Ready(code.original_bytes()),
                None => AccountCodeSource::Hash(account.info.code_hash),
            },
            None => AccountCodeSource::Unloaded,
        };

        match source {
            AccountCodeSource::Ready(code) => Ok(code),
            AccountCodeSource::Hash(code_hash) => {
                Self::code_by_hash_without_warming(context, code_hash)
            }
            AccountCodeSource::Unloaded => {
                let Some(info) = context.db_mut().basic(account)? else {
                    return Ok(Bytes::new());
                };
                match info.code {
                    Some(code) => Ok(code.original_bytes()),
                    None => Self::code_by_hash_without_warming(context, info.code_hash),
                }
            }
        }
    }

    fn code_by_hash_without_warming<DB: Database>(
        context: &mut ArbitrumContext<DB>,
        code_hash: B256,
    ) -> Result<Bytes, DB::Error> {
        if code_hash == B256::ZERO || code_hash == KECCAK_EMPTY {
            return Ok(Bytes::new());
        }
        context
            .db_mut()
            .code_by_hash(code_hash)
            .map(|code| code.original_bytes())
    }
}

enum AccountCodeSource {
    Ready(Bytes),
    Hash(B256),
    Unloaded,
}

#[cfg(test)]
mod tests {
    use super::super::BASE_PRECOMPILE_GAS;
    use super::super::util::copy_gas;
    use super::*;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::primitives::{Address, Bytes, U256};
    use alloy::sol_types::SolCall;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::database::EmptyDB;
    use revm::database::in_memory_db::CacheDB;
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
    fn account_reads_do_not_warm_accounts() {
        let account = Address::from([0x44; 20]);
        let code = Bytes::from_static(&[0x60, 0x00]);
        let mut context = context_with_account(account, U256::from(123), code.clone());
        let balance_data = IArbInfo::getBalanceCall { account }.abi_encode();
        let code_data = IArbInfo::getCodeCall { account }.abi_encode();

        assert!(!context.journal().state.contains_key(&account));

        ArbInfo::run(input(&balance_data, 10_000, &mut context)).expect("getBalance");
        assert!(!context.journal().state.contains_key(&account));

        ArbInfo::run(input(&code_data, 10_000, &mut context)).expect("getCode");
        assert!(!context.journal().state.contains_key(&account));
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

    #[derive(Debug)]
    struct ExpectedDbError;

    impl core::fmt::Display for ExpectedDbError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("expected database error")
        }
    }

    impl std::error::Error for ExpectedDbError {}
    impl revm::database_interface::DBErrorMarker for ExpectedDbError {}

    struct FailingDb;

    impl Database for FailingDb {
        type Error = ExpectedDbError;

        fn basic(&mut self, _: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(ExpectedDbError)
        }

        fn code_by_hash(&mut self, _: B256) -> Result<Bytecode, Self::Error> {
            Err(ExpectedDbError)
        }

        fn storage(&mut self, _: Address, _: U256) -> Result<U256, Self::Error> {
            Err(ExpectedDbError)
        }

        fn block_hash(&mut self, _: u64) -> Result<B256, Self::Error> {
            Err(ExpectedDbError)
        }
    }

    #[test]
    fn db_errors_are_fatal() {
        let account = Address::from([0x55; 20]);
        let data = IArbInfo::getBalanceCall { account }.abi_encode();
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(FailingDb)
            .with_chain(ArbitrumExecutionContext::default());

        let error = ArbInfo::run(input(&data, 10_000, &mut context)).expect_err("DB should fail");

        match error {
            PrecompileError::Fatal(message) => assert_eq!(message, "ExpectedDbError"),
            error => panic!("database failure was not fatal: {error:?}"),
        }
    }
}
