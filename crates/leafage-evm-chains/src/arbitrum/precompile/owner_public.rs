use super::abi::IArbOwnerPublic;
use super::state::ArbStorage;
use super::util::{dispatch, empty_revert, finish_call, log_gas};
use super::{ArbPrecompileInput, ArbitrumContext};
use crate::arbitrum::arbos_state;
use alloy::primitives::{keccak256, Address, Bytes, Log};
use alloy::sol_types::SolValue;
use revm::context::{ContextTr, JournalTr};
use revm::precompile::{PrecompileError, PrecompileResult};
use revm::Database;

const ARBOS_VERSION_5: u64 = 5;
const ARBOS_VERSION_11: u64 = 11;
const ARBOS_VERSION_20: u64 = 20;
const ARBOS_VERSION_40: u64 = 40;
const ARBOS_VERSION_41: u64 = 41;
const ARBOS_VERSION_50: u64 = 50;
const ARBOS_VERSION_60: u64 = 60;

pub(super) struct ArbOwnerPublic;

impl ArbOwnerPublic {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let is_static = input.is_static;
        let current_arbos_version = input.current_arbos_version;
        let context = input.context;
        dispatch::<IArbOwnerPublic::IArbOwnerPublicCalls>(data, gas_limit, |call, initial_gas| {
            if current_arbos_version < Self::required_arbos_version(&call) {
                return empty_revert(gas_limit, gas_limit);
            }

            let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
            match call {
                IArbOwnerPublic::IArbOwnerPublicCalls::isChainOwner(call) => {
                    let owners_key = storage.chain_owner_key();
                    let ret = storage.address_set_contains(&owners_key, call.addr)?;
                    finish_call::<IArbOwnerPublic::isChainOwnerCall>(
                        gas_limit,
                        storage.gas_used,
                        ret.into(),
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getAllChainOwners(_) => {
                    let owners_key = storage.chain_owner_key();
                    let ret = storage.address_set_members(&owners_key)?;
                    finish_call::<IArbOwnerPublic::getAllChainOwnersCall>(
                        gas_limit,
                        storage.gas_used,
                        ret.into(),
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getNativeTokenManagementFrom(_) => {
                    let ret = storage
                        .root(arbos_state::NATIVE_TOKEN_ENABLED_FROM_TIME_OFFSET)?
                        .to::<u64>();
                    finish_call::<IArbOwnerPublic::getNativeTokenManagementFromCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::isNativeTokenOwner(call) => {
                    let owners_key = storage.native_token_owner_key();
                    let ret = storage.address_set_contains(&owners_key, call.addr)?;
                    finish_call::<IArbOwnerPublic::isNativeTokenOwnerCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getAllNativeTokenOwners(_) => {
                    let owners_key = storage.native_token_owner_key();
                    let ret = storage.address_set_members(&owners_key)?;
                    finish_call::<IArbOwnerPublic::getAllNativeTokenOwnersCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getTransactionFilteringFrom(_) => {
                    let ret = storage
                        .root(arbos_state::TRANSACTION_FILTERING_ENABLED_FROM_TIME_OFFSET)?
                        .to::<u64>();
                    finish_call::<IArbOwnerPublic::getTransactionFilteringFromCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::isTransactionFilterer(call) => {
                    let filterers_key = storage.transaction_filterer_key();
                    let ret = storage.address_set_contains(&filterers_key, call.filterer)?;
                    finish_call::<IArbOwnerPublic::isTransactionFiltererCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getAllTransactionFilterers(_) => {
                    let filterers_key = storage.transaction_filterer_key();
                    let ret = storage.address_set_members(&filterers_key)?;
                    finish_call::<IArbOwnerPublic::getAllTransactionFilterersCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getFilteredFundsRecipient(_) => {
                    let ret =
                        storage.read_address(&[], arbos_state::FILTERED_FUNDS_RECIPIENT_OFFSET)?;
                    finish_call::<IArbOwnerPublic::getFilteredFundsRecipientCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getNetworkFeeAccount(_) => {
                    let ret = storage.read_address(&[], arbos_state::NETWORK_FEE_ACCOUNT_OFFSET)?;
                    finish_call::<IArbOwnerPublic::getNetworkFeeAccountCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getInfraFeeAccount(_) => {
                    let offset = if storage.arbos_version()? < 6 {
                        arbos_state::NETWORK_FEE_ACCOUNT_OFFSET
                    } else {
                        arbos_state::INFRA_FEE_ACCOUNT_OFFSET
                    };
                    let ret = storage.read_address(&[], offset)?;
                    finish_call::<IArbOwnerPublic::getInfraFeeAccountCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getBrotliCompressionLevel(_) => {
                    let ret = storage
                        .root(arbos_state::BROTLI_COMPRESSION_LEVEL_OFFSET)?
                        .to::<u64>();
                    finish_call::<IArbOwnerPublic::getBrotliCompressionLevelCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getParentGasFloorPerToken(_) => {
                    let l1_key = storage.l1_key();
                    let ret = storage
                        .read(&l1_key, arbos_state::L1_GAS_FLOOR_PER_TOKEN_OFFSET)?
                        .to::<u64>();
                    finish_call::<IArbOwnerPublic::getParentGasFloorPerTokenCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getScheduledUpgrade(_) => {
                    let current = storage.arbos_version()?;
                    let version = storage
                        .root(arbos_state::UPGRADE_VERSION_OFFSET)?
                        .to::<u64>();
                    let timestamp = storage
                        .root(arbos_state::UPGRADE_TIMESTAMP_OFFSET)?
                        .to::<u64>();
                    let ret = if version <= current {
                        (0, 0)
                    } else {
                        (version, timestamp)
                    };
                    finish_call::<IArbOwnerPublic::getScheduledUpgradeCall>(
                        gas_limit,
                        storage.gas_used,
                        ret.into(),
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::isCalldataPriceIncreaseEnabled(_) => {
                    let ret = storage.calldata_price_increase_enabled()?;
                    finish_call::<IArbOwnerPublic::isCalldataPriceIncreaseEnabledCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getCollectTips(_) => {
                    let ret = storage.collect_tips()?;
                    finish_call::<IArbOwnerPublic::getCollectTipsCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::getMaxStylusContractFragments(_) => {
                    let ret = storage.max_stylus_contract_fragments()?;
                    finish_call::<IArbOwnerPublic::getMaxStylusContractFragmentsCall>(
                        gas_limit,
                        storage.gas_used,
                        ret,
                    )
                }
                IArbOwnerPublic::IArbOwnerPublicCalls::rectifyChainOwner(call) => {
                    if is_static {
                        return empty_revert(gas_limit, storage.gas_used);
                    }
                    let owners_key = storage.chain_owner_key();
                    match storage.address_set_rectify_mapping(&owners_key, call.ownerToRectify) {
                        Ok(()) => {
                            Self::emit_chain_owner_rectified(&mut storage, call.ownerToRectify)?;
                            finish_call::<IArbOwnerPublic::rectifyChainOwnerCall>(
                                gas_limit,
                                storage.gas_used,
                                ().into(),
                            )
                        }
                        Err(PrecompileError::Other(_)) => empty_revert(gas_limit, storage.gas_used),
                        Err(error) => Err(error),
                    }
                }
            }
        })
    }

    fn required_arbos_version(call: &IArbOwnerPublic::IArbOwnerPublicCalls) -> u64 {
        match call {
            IArbOwnerPublic::IArbOwnerPublicCalls::getInfraFeeAccount(_) => ARBOS_VERSION_5,
            IArbOwnerPublic::IArbOwnerPublicCalls::rectifyChainOwner(_) => ARBOS_VERSION_11,
            IArbOwnerPublic::IArbOwnerPublicCalls::getBrotliCompressionLevel(_)
            | IArbOwnerPublic::IArbOwnerPublicCalls::getScheduledUpgrade(_) => ARBOS_VERSION_20,
            IArbOwnerPublic::IArbOwnerPublicCalls::isCalldataPriceIncreaseEnabled(_) => {
                ARBOS_VERSION_40
            }
            IArbOwnerPublic::IArbOwnerPublicCalls::isNativeTokenOwner(_)
            | IArbOwnerPublic::IArbOwnerPublicCalls::getAllNativeTokenOwners(_) => ARBOS_VERSION_41,
            IArbOwnerPublic::IArbOwnerPublicCalls::getNativeTokenManagementFrom(_)
            | IArbOwnerPublic::IArbOwnerPublicCalls::getParentGasFloorPerToken(_) => {
                ARBOS_VERSION_50
            }
            IArbOwnerPublic::IArbOwnerPublicCalls::getTransactionFilteringFrom(_)
            | IArbOwnerPublic::IArbOwnerPublicCalls::isTransactionFilterer(_)
            | IArbOwnerPublic::IArbOwnerPublicCalls::getAllTransactionFilterers(_)
            | IArbOwnerPublic::IArbOwnerPublicCalls::getFilteredFundsRecipient(_)
            | IArbOwnerPublic::IArbOwnerPublicCalls::getCollectTips(_)
            | IArbOwnerPublic::IArbOwnerPublicCalls::getMaxStylusContractFragments(_) => {
                ARBOS_VERSION_60
            }
            _ => 0,
        }
    }

    fn emit_chain_owner_rectified<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        owner: Address,
    ) -> Result<(), PrecompileError> {
        let data = Bytes::from((owner,).abi_encode());
        storage.burn(log_gas(0, data.len()))?;
        storage.context.journal_mut().log(Log::new_unchecked(
            super::ARB_OWNER_PUBLIC_ADDRESS,
            vec![keccak256("ChainOwnerRectified(address)")],
            data,
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrum::context::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::primitives::{Address, U256};
    use alloy::sol_types::SolCall;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::context::JournalTr;
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::{Context, MainContext};

    fn context() -> ArbitrumContext<CacheDB<EmptyDB>> {
        let db = CacheDB::new(EmptyDB::default());
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
        context
    }

    fn corrupt_chain_owner_list_slot<DB: Database>(
        context: &mut ArbitrumContext<DB>,
        owner: Address,
    ) {
        let mut storage = ArbStorage::new_with_initial_gas(context, u64::MAX, 0);
        let owners_key = storage.chain_owner_key();
        storage
            .address_set_add(&owners_key, owner)
            .expect("add chain owner");
        storage
            .write(&owners_key, 1, U256::ZERO)
            .expect("corrupt chain owner slot");
    }

    fn run_rectify(
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
        owner: Address,
        gas: u64,
    ) -> PrecompileResult {
        let data = IArbOwnerPublic::rectifyChainOwnerCall {
            ownerToRectify: owner,
        }
        .abi_encode();
        ArbOwnerPublic::run(ArbPrecompileInput {
            data: &data,
            gas,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: ARBOS_VERSION_11,
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
    fn method_versions_match_nitro_registration() {
        let address = Address::ZERO;
        let cases = [
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::getInfraFeeAccount(
                    IArbOwnerPublic::getInfraFeeAccountCall {},
                ),
                ARBOS_VERSION_5,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::rectifyChainOwner(
                    IArbOwnerPublic::rectifyChainOwnerCall {
                        ownerToRectify: address,
                    },
                ),
                ARBOS_VERSION_11,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::getBrotliCompressionLevel(
                    IArbOwnerPublic::getBrotliCompressionLevelCall {},
                ),
                ARBOS_VERSION_20,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::getScheduledUpgrade(
                    IArbOwnerPublic::getScheduledUpgradeCall {},
                ),
                ARBOS_VERSION_20,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::isCalldataPriceIncreaseEnabled(
                    IArbOwnerPublic::isCalldataPriceIncreaseEnabledCall {},
                ),
                ARBOS_VERSION_40,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::isNativeTokenOwner(
                    IArbOwnerPublic::isNativeTokenOwnerCall { addr: address },
                ),
                ARBOS_VERSION_41,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::getAllNativeTokenOwners(
                    IArbOwnerPublic::getAllNativeTokenOwnersCall {},
                ),
                ARBOS_VERSION_41,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::getNativeTokenManagementFrom(
                    IArbOwnerPublic::getNativeTokenManagementFromCall {},
                ),
                ARBOS_VERSION_50,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::getParentGasFloorPerToken(
                    IArbOwnerPublic::getParentGasFloorPerTokenCall {},
                ),
                ARBOS_VERSION_50,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::getTransactionFilteringFrom(
                    IArbOwnerPublic::getTransactionFilteringFromCall {},
                ),
                ARBOS_VERSION_60,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::isTransactionFilterer(
                    IArbOwnerPublic::isTransactionFiltererCall { filterer: address },
                ),
                ARBOS_VERSION_60,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::getAllTransactionFilterers(
                    IArbOwnerPublic::getAllTransactionFilterersCall {},
                ),
                ARBOS_VERSION_60,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::getFilteredFundsRecipient(
                    IArbOwnerPublic::getFilteredFundsRecipientCall {},
                ),
                ARBOS_VERSION_60,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::getCollectTips(
                    IArbOwnerPublic::getCollectTipsCall {},
                ),
                ARBOS_VERSION_60,
            ),
            (
                IArbOwnerPublic::IArbOwnerPublicCalls::getMaxStylusContractFragments(
                    IArbOwnerPublic::getMaxStylusContractFragmentsCall {},
                ),
                ARBOS_VERSION_60,
            ),
        ];

        for (call, version) in cases {
            assert_eq!(ArbOwnerPublic::required_arbos_version(&call), version);
        }

        assert_eq!(
            ArbOwnerPublic::required_arbos_version(
                &IArbOwnerPublic::IArbOwnerPublicCalls::isChainOwner(
                    IArbOwnerPublic::isChainOwnerCall { addr: address }
                ),
            ),
            0
        );
    }

    #[test]
    fn chain_owner_rectified_event_burns_gas_before_log() {
        let owner = Address::from([0x11; 20]);
        let event_gas = log_gas(0, 32);
        let mut context = context();

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, event_gas - 1, 0);
            let error = ArbOwnerPublic::emit_chain_owner_rectified(&mut storage, owner)
                .expect_err("event should run out of gas");
            assert!(error.is_oog());
        }
        assert!(context.journal_mut().take_logs().is_empty());

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, event_gas, 0);
            ArbOwnerPublic::emit_chain_owner_rectified(&mut storage, owner).expect("emit event");
            assert_eq!(storage.gas_used, event_gas);
        }

        let logs = context.journal_mut().take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, super::super::ARB_OWNER_PUBLIC_ADDRESS);
        assert_eq!(
            logs[0].data.topics()[0],
            keccak256("ChainOwnerRectified(address)")
        );
        assert_eq!(logs[0].data.data, Bytes::from((owner,).abi_encode()));
    }

    #[test]
    fn rectify_chain_owner_charges_and_emits_rectified_event() {
        let owner = Address::from([0x22; 20]);
        let mut context = context();
        corrupt_chain_owner_list_slot(&mut context, owner);

        let output = run_rectify(&mut context, owner, 1_000_000).expect("rectify should succeed");

        assert!(!output.reverted);
        assert!(output.bytes.is_empty());
        assert!(output.gas_used >= log_gas(0, 32));

        let logs = context.journal_mut().take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].address, super::super::ARB_OWNER_PUBLIC_ADDRESS);
        assert_eq!(
            logs[0].data.topics()[0],
            keccak256("ChainOwnerRectified(address)")
        );
        assert_eq!(logs[0].data.data, Bytes::from((owner,).abi_encode()));
    }
}
