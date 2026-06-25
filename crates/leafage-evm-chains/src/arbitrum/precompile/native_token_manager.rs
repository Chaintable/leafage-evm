use super::abi::IArbNativeTokenManager;
use super::state::ArbStorage;
use super::util::{dispatch, empty_revert, finish_call, log_gas, topic_address};
use super::{ArbPrecompileInput, ArbitrumContext, ARB_NATIVE_TOKEN_MANAGER_ADDRESS};
use alloy::primitives::{keccak256, Address, Bytes, Log, U256};
use alloy::sol_types::SolValue;
use revm::context::{ContextTr, JournalTr};
use revm::precompile::{PrecompileError, PrecompileResult};
use revm::Database;

pub(super) struct ArbNativeTokenManager;

const MINT_BURN_GAS_COST: u64 = 9_100;

impl ArbNativeTokenManager {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let caller = input.caller;
        let is_static = input.is_static;
        let context = input.context;
        dispatch::<IArbNativeTokenManager::IArbNativeTokenManagerCalls>(
            data,
            gas_limit,
            |call, initial_gas| {
                let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
                if is_static {
                    return empty_revert(gas_limit, storage.gas_used);
                }
                if !Self::has_access(&mut storage, caller)? {
                    return Err(PrecompileError::OutOfGas);
                }

                match call {
                    IArbNativeTokenManager::IArbNativeTokenManagerCalls::mintNativeToken(call) => {
                        storage.burn(MINT_BURN_GAS_COST)?;
                        storage.mint_balance(caller, call.amount)?;
                        Self::emit(
                            &mut storage,
                            "NativeTokenMinted(address,uint256)",
                            caller,
                            call.amount,
                        )?;
                        finish_call::<IArbNativeTokenManager::mintNativeTokenCall>(
                            gas_limit,
                            storage.gas_used,
                            ().into(),
                        )
                    }
                    IArbNativeTokenManager::IArbNativeTokenManagerCalls::burnNativeToken(call) => {
                        storage.burn(MINT_BURN_GAS_COST)?;
                        match storage.burn_balance(caller, call.amount) {
                            Ok(()) => {
                                Self::emit(
                                    &mut storage,
                                    "NativeTokenBurned(address,uint256)",
                                    caller,
                                    call.amount,
                                )?;
                                finish_call::<IArbNativeTokenManager::burnNativeTokenCall>(
                                    gas_limit,
                                    storage.gas_used,
                                    ().into(),
                                )
                            }
                            Err(PrecompileError::Other(_)) => {
                                empty_revert(gas_limit, storage.gas_used)
                            }
                            Err(error) => Err(error),
                        }
                    }
                }
            },
        )
    }

    fn has_access<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
    ) -> Result<bool, PrecompileError> {
        let owners_key = storage.native_token_owner_key();
        storage.address_set_contains(&owners_key, caller)
    }

    fn emit<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        event: &'static str,
        account: Address,
        amount: U256,
    ) -> Result<(), PrecompileError> {
        let data = Bytes::from((amount,).abi_encode());
        storage.burn(log_gas(1, data.len()))?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_NATIVE_TOKEN_MANAGER_ADDRESS,
            vec![keccak256(event), topic_address(account)],
            data,
        ));
        Ok(())
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
    use alloy::sol_types::SolCall;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::context::JournalTr;
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::precompile::PrecompileOutput;
    use revm::state::AccountInfo;
    use revm::{Context, MainContext};

    const WORD_COPY_GAS: u64 = 3;

    fn context_with_native_token_owner(
        owner: Address,
        balance: U256,
    ) -> ArbitrumContext<CacheDB<EmptyDB>> {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            owner,
            AccountInfo {
                balance,
                ..Default::default()
            },
        );
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(db)
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            let owners_key = storage.native_token_owner_key();
            storage
                .address_set_add(&owners_key, owner)
                .expect("add native token owner");
        }
        context
    }

    fn run_call(
        data: &[u8],
        caller: Address,
        gas: u64,
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
    ) -> PrecompileResult {
        ArbNativeTokenManager::run(ArbPrecompileInput {
            data,
            gas,
            caller,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 60,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context,
        })
    }

    #[test]
    fn mint_native_token_charges_nitro_mint_and_event_gas() {
        let caller = Address::from([0x11; 20]);
        let mut context = context_with_native_token_owner(caller, U256::ZERO);
        let amount = U256::from(7);
        let input = IArbNativeTokenManager::mintNativeTokenCall { amount }.abi_encode();

        let output: PrecompileOutput = run_call(&input, caller, 1_000_000, &mut context).unwrap();

        assert!(!output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(
            output.gas_used,
            BASE_PRECOMPILE_GAS
                + WORD_COPY_GAS
                + STORAGE_READ_GAS
                + MINT_BURN_GAS_COST
                + log_gas(1, 32)
        );

        let logs = context.journal_mut().take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].data.topics()[0],
            keccak256("NativeTokenMinted(address,uint256)")
        );
        assert_eq!(logs[0].data.topics()[1], topic_address(caller));
    }

    #[test]
    fn burn_native_token_charges_nitro_burn_and_event_gas() {
        let caller = Address::from([0x12; 20]);
        let mut context = context_with_native_token_owner(caller, U256::from(10));
        let amount = U256::from(4);
        let input = IArbNativeTokenManager::burnNativeTokenCall { amount }.abi_encode();

        let output: PrecompileOutput = run_call(&input, caller, 1_000_000, &mut context).unwrap();

        assert!(!output.reverted);
        assert!(output.bytes.is_empty());
        assert_eq!(
            output.gas_used,
            BASE_PRECOMPILE_GAS
                + WORD_COPY_GAS
                + STORAGE_READ_GAS
                + MINT_BURN_GAS_COST
                + log_gas(1, 32)
        );

        let logs = context.journal_mut().take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].data.topics()[0],
            keccak256("NativeTokenBurned(address,uint256)")
        );
        assert_eq!(logs[0].data.topics()[1], topic_address(caller));
    }

    #[test]
    fn mint_native_token_oog_before_log_does_not_emit_event() {
        let caller = Address::from([0x22; 20]);
        let mut context = context_with_native_token_owner(caller, U256::ZERO);
        let input = IArbNativeTokenManager::mintNativeTokenCall {
            amount: U256::from(1),
        }
        .abi_encode();
        let gas_before_event =
            BASE_PRECOMPILE_GAS + WORD_COPY_GAS + STORAGE_READ_GAS + MINT_BURN_GAS_COST;

        let error = run_call(
            &input,
            caller,
            gas_before_event + log_gas(1, 32) - 1,
            &mut context,
        )
        .expect_err("event gas should run out");

        assert!(error.is_oog());
        assert!(context.journal_mut().take_logs().is_empty());
    }
}
