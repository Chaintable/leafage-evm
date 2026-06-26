use super::abi::IArbOwner;
use super::chain_config::NitroChainConfig;
use super::state::{ArbStorage, MultiGasPricingConstraint};
use super::util::{dispatch, empty_revert, finish_call, topic_address};
use super::{
    ArbPrecompileInput, ArbitrumContext, ARBOS_VERSION_MULTI_GAS_CONSTRAINTS, ARB_OWNER_ADDRESS,
    NUM_RESOURCE_KIND, RESOURCE_KIND_SINGLE_DIM,
};
use crate::arbitrum::arbos_state;
use alloy::primitives::{keccak256, Address, Bytes, Log, B256, U256};
use alloy::sol_types::{SolCall, SolValue};
use revm::context::{ContextTr, JournalTr};
use revm::context_interface::Block;
use revm::precompile::{PrecompileError, PrecompileResult};
use revm::Database;

const FEATURE_ENABLE_DELAY: u64 = 7 * 24 * 60 * 60;
const MAX_UINT24: u32 = 0x00ff_ffff;
const MIN_INIT_GAS_UNITS: u64 = 128;
const MIN_CACHED_GAS_UNITS: u64 = 32;
const COST_SCALAR_PERCENT: u64 = 2;
const ARBOS_VERSION_5: u64 = 5;
const ARBOS_VERSION_10: u64 = 10;
const ARBOS_VERSION_11: u64 = 11;
const ARBOS_VERSION_20: u64 = 20;
const ARBOS_VERSION_STYLUS: u64 = 30;
const ARBOS_VERSION_40: u64 = 40;
const ARBOS_VERSION_41: u64 = 41;
const ARBOS_VERSION_50: u64 = 50;
const ARBOS_VERSION_59: u64 = 59;
const ARBOS_VERSION_MULTI_CONSTRAINT_FIX: u64 = 51;
const MAX_GAS_PRICING_CONSTRAINTS: usize = 20;
const MAX_PRICING_EXPONENT_BIPS: i64 = 85_000;
const ONE_IN_BIPS: i64 = 10_000;
const RESOURCE_KIND_UNKNOWN: usize = 0;

pub(super) struct ArbOwner;

impl ArbOwner {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let caller = input.caller;
        let value = input.value;
        let is_static = input.is_static;
        let is_valid_call_context = input.is_valid_call_context;
        let current_arbos_version = input.current_arbos_version;
        let current_chain_config = input.current_chain_config;
        let context = input.context;

        {
            let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, 0);
            if !Self::caller_is_owner(&mut storage, caller)? {
                return empty_revert(gas_limit, storage.gas_used);
            }
        }

        if !is_valid_call_context || !value.is_zero() {
            return Self::finish_owner_call(gas_limit, empty_revert(gas_limit, gas_limit));
        }

        let result = dispatch::<IArbOwner::IArbOwnerCalls>(data, gas_limit, |call, initial_gas| {
            if !Self::method_is_available(current_arbos_version, &call) {
                return empty_revert(gas_limit, gas_limit);
            }

            let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);

            match call {
                IArbOwner::IArbOwnerCalls::isChainOwner(call) => {
                    let owners_key = storage.chain_owner_key();
                    let ret = storage.address_set_contains(&owners_key, call.addr)?;
                    Self::finish_view::<IArbOwner::isChainOwnerCall, DB>(
                        &mut storage,
                        gas_limit,
                        data,
                        caller,
                        is_static,
                        current_arbos_version,
                        ret,
                    )
                }
                IArbOwner::IArbOwnerCalls::getAllChainOwners(_) => {
                    let owners_key = storage.chain_owner_key();
                    let ret = storage.address_set_members(&owners_key)?;
                    Self::finish_view::<IArbOwner::getAllChainOwnersCall, DB>(
                        &mut storage,
                        gas_limit,
                        data,
                        caller,
                        is_static,
                        current_arbos_version,
                        ret,
                    )
                }
                IArbOwner::IArbOwnerCalls::isNativeTokenOwner(call) => {
                    let owners_key = storage.native_token_owner_key();
                    let ret = storage.address_set_contains(&owners_key, call.addr)?;
                    Self::finish_view::<IArbOwner::isNativeTokenOwnerCall, DB>(
                        &mut storage,
                        gas_limit,
                        data,
                        caller,
                        is_static,
                        current_arbos_version,
                        ret,
                    )
                }
                IArbOwner::IArbOwnerCalls::getAllNativeTokenOwners(_) => {
                    let owners_key = storage.native_token_owner_key();
                    let ret = storage.address_set_members(&owners_key)?;
                    Self::finish_view::<IArbOwner::getAllNativeTokenOwnersCall, DB>(
                        &mut storage,
                        gas_limit,
                        data,
                        caller,
                        is_static,
                        current_arbos_version,
                        ret,
                    )
                }
                IArbOwner::IArbOwnerCalls::isTransactionFilterer(call) => {
                    let filterers_key = storage.transaction_filterer_key();
                    let ret = storage.address_set_contains(&filterers_key, call.filterer)?;
                    Self::finish_view::<IArbOwner::isTransactionFiltererCall, DB>(
                        &mut storage,
                        gas_limit,
                        data,
                        caller,
                        is_static,
                        current_arbos_version,
                        ret,
                    )
                }
                IArbOwner::IArbOwnerCalls::getAllTransactionFilterers(_) => {
                    let filterers_key = storage.transaction_filterer_key();
                    let ret = storage.address_set_members(&filterers_key)?;
                    Self::finish_view::<IArbOwner::getAllTransactionFilterersCall, DB>(
                        &mut storage,
                        gas_limit,
                        data,
                        caller,
                        is_static,
                        current_arbos_version,
                        ret,
                    )
                }
                IArbOwner::IArbOwnerCalls::getFilteredFundsRecipient(_) => {
                    let ret =
                        storage.read_address(&[], arbos_state::FILTERED_FUNDS_RECIPIENT_OFFSET)?;
                    Self::finish_view::<IArbOwner::getFilteredFundsRecipientCall, DB>(
                        &mut storage,
                        gas_limit,
                        data,
                        caller,
                        is_static,
                        current_arbos_version,
                        ret,
                    )
                }
                IArbOwner::IArbOwnerCalls::getNetworkFeeAccount(_) => {
                    let ret = storage.read_address(&[], arbos_state::NETWORK_FEE_ACCOUNT_OFFSET)?;
                    Self::finish_view::<IArbOwner::getNetworkFeeAccountCall, DB>(
                        &mut storage,
                        gas_limit,
                        data,
                        caller,
                        is_static,
                        current_arbos_version,
                        ret,
                    )
                }
                IArbOwner::IArbOwnerCalls::getInfraFeeAccount(_) => {
                    let offset = if current_arbos_version < 6 {
                        arbos_state::NETWORK_FEE_ACCOUNT_OFFSET
                    } else {
                        arbos_state::INFRA_FEE_ACCOUNT_OFFSET
                    };
                    let ret = storage.read_address(&[], offset)?;
                    Self::finish_view::<IArbOwner::getInfraFeeAccountCall, DB>(
                        &mut storage,
                        gas_limit,
                        data,
                        caller,
                        is_static,
                        current_arbos_version,
                        ret,
                    )
                }
                _ => Self::run_write(
                    &mut storage,
                    gas_limit,
                    data,
                    caller,
                    is_static,
                    current_arbos_version,
                    current_chain_config,
                    call,
                ),
            }
        });

        Self::finish_owner_call(gas_limit, result)
    }

    fn run_write<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        gas_limit: u64,
        data: &[u8],
        caller: Address,
        is_static: bool,
        current_arbos_version: u64,
        current_chain_config: Option<&[u8]>,
        call: IArbOwner::IArbOwnerCalls,
    ) -> PrecompileResult {
        if is_static {
            return empty_revert(gas_limit, storage.gas_used);
        }

        let result = match call {
            IArbOwner::IArbOwnerCalls::addChainOwner(call) => {
                let owners_key = storage.chain_owner_key();
                storage.address_set_add(&owners_key, call.newOwner)?;
                if current_arbos_version >= 60 {
                    Self::emit_address(storage, "ChainOwnerAdded(address)", call.newOwner);
                }
                Self::finish_write::<IArbOwner::addChainOwnerCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::removeChainOwner(call) => {
                let owners_key = storage.chain_owner_key();
                if !storage.address_set_contains(&owners_key, call.ownerToRemove)? {
                    return empty_revert(gas_limit, storage.gas_used);
                }
                storage.address_set_remove(&owners_key, call.ownerToRemove)?;
                if current_arbos_version >= 60 {
                    Self::emit_address(storage, "ChainOwnerRemoved(address)", call.ownerToRemove);
                }
                Self::finish_write::<IArbOwner::removeChainOwnerCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setNativeTokenManagementFrom(call) => {
                Self::set_feature_from_time(
                    storage,
                    arbos_state::NATIVE_TOKEN_ENABLED_FROM_TIME_OFFSET,
                    call.timestamp,
                )?;
                Self::finish_write::<IArbOwner::setNativeTokenManagementFromCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setTransactionFilteringFrom(call) => {
                Self::set_feature_from_time(
                    storage,
                    arbos_state::TRANSACTION_FILTERING_ENABLED_FROM_TIME_OFFSET,
                    call.timestamp,
                )?;
                Self::finish_write::<IArbOwner::setTransactionFilteringFromCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::addNativeTokenOwner(call) => {
                Self::ensure_feature_enabled(
                    storage,
                    arbos_state::NATIVE_TOKEN_ENABLED_FROM_TIME_OFFSET,
                    "native token feature is not enabled yet",
                )?;
                let owners_key = storage.native_token_owner_key();
                storage.address_set_add(&owners_key, call.newOwner)?;
                if current_arbos_version >= 60 {
                    Self::emit_address(storage, "NativeTokenOwnerAdded(address)", call.newOwner);
                }
                Self::finish_write::<IArbOwner::addNativeTokenOwnerCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::removeNativeTokenOwner(call) => {
                let owners_key = storage.native_token_owner_key();
                if !storage.address_set_contains(&owners_key, call.ownerToRemove)? {
                    return empty_revert(gas_limit, storage.gas_used);
                }
                storage.address_set_remove(&owners_key, call.ownerToRemove)?;
                if current_arbos_version >= 60 {
                    Self::emit_address(
                        storage,
                        "NativeTokenOwnerRemoved(address)",
                        call.ownerToRemove,
                    );
                }
                Self::finish_write::<IArbOwner::removeNativeTokenOwnerCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::addTransactionFilterer(call) => {
                Self::ensure_feature_enabled(
                    storage,
                    arbos_state::TRANSACTION_FILTERING_ENABLED_FROM_TIME_OFFSET,
                    "transaction filtering feature is not enabled yet",
                )?;
                let filterers_key = storage.transaction_filterer_key();
                storage.address_set_add(&filterers_key, call.filterer)?;
                Self::emit_address(storage, "TransactionFiltererAdded(address)", call.filterer);
                Self::finish_write::<IArbOwner::addTransactionFiltererCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::removeTransactionFilterer(call) => {
                let filterers_key = storage.transaction_filterer_key();
                if !storage.address_set_contains(&filterers_key, call.filterer)? {
                    return empty_revert(gas_limit, storage.gas_used);
                }
                storage.address_set_remove(&filterers_key, call.filterer)?;
                Self::emit_address(
                    storage,
                    "TransactionFiltererRemoved(address)",
                    call.filterer,
                );
                Self::finish_write::<IArbOwner::removeTransactionFiltererCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setFilteredFundsRecipient(call) => {
                storage.write_address(
                    &[],
                    arbos_state::FILTERED_FUNDS_RECIPIENT_OFFSET,
                    call.newRecipient,
                )?;
                Self::emit_address(
                    storage,
                    "FilteredFundsRecipientSet(address)",
                    call.newRecipient,
                );
                Self::finish_write::<IArbOwner::setFilteredFundsRecipientCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setNetworkFeeAccount(call) => {
                storage.write_address(
                    &[],
                    arbos_state::NETWORK_FEE_ACCOUNT_OFFSET,
                    call.newNetworkFeeAccount,
                )?;
                Self::finish_write::<IArbOwner::setNetworkFeeAccountCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setInfraFeeAccount(call) => {
                storage.write_address(
                    &[],
                    arbos_state::INFRA_FEE_ACCOUNT_OFFSET,
                    call.newInfraFeeAccount,
                )?;
                Self::finish_write::<IArbOwner::setInfraFeeAccountCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::scheduleArbOSUpgrade(call) => {
                storage.write(
                    &[],
                    arbos_state::UPGRADE_VERSION_OFFSET,
                    U256::from(call.newVersion),
                )?;
                storage.write(
                    &[],
                    arbos_state::UPGRADE_TIMESTAMP_OFFSET,
                    U256::from(call.timestamp),
                )?;
                Self::finish_write::<IArbOwner::scheduleArbOSUpgradeCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setL1BaseFeeEstimateInertia(call) => {
                let l1_key = storage.l1_key();
                storage.write(
                    &l1_key,
                    arbos_state::L1_INERTIA_OFFSET,
                    U256::from(call.inertia),
                )?;
                Self::finish_write::<IArbOwner::setL1BaseFeeEstimateInertiaCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setL2BaseFee(call) => {
                let l2_key = storage.l2_key();
                storage.write(
                    &l2_key,
                    arbos_state::L2_BASE_FEE_WEI_OFFSET,
                    call.priceInWei,
                )?;
                Self::finish_write::<IArbOwner::setL2BaseFeeCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setMinimumL2BaseFee(call) => {
                let l2_key = storage.l2_key();
                storage.write(
                    &l2_key,
                    arbos_state::L2_MIN_BASE_FEE_WEI_OFFSET,
                    call.priceInWei,
                )?;
                Self::finish_write::<IArbOwner::setMinimumL2BaseFeeCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setSpeedLimit(call) => {
                if call.limit == 0 {
                    return empty_revert(gas_limit, storage.gas_used);
                }
                let l2_key = storage.l2_key();
                storage.write(
                    &l2_key,
                    arbos_state::L2_SPEED_LIMIT_PER_SECOND_OFFSET,
                    U256::from(call.limit),
                )?;
                Self::finish_write::<IArbOwner::setSpeedLimitCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setMaxTxGasLimit(call) => {
                let l2_key = storage.l2_key();
                let offset = if current_arbos_version < 50 {
                    arbos_state::L2_PER_BLOCK_GAS_LIMIT_OFFSET
                } else {
                    arbos_state::L2_PER_TX_GAS_LIMIT_OFFSET
                };
                storage.write(&l2_key, offset, U256::from(call.limit))?;
                Self::finish_write::<IArbOwner::setMaxTxGasLimitCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setMaxBlockGasLimit(call) => {
                let l2_key = storage.l2_key();
                storage.write(
                    &l2_key,
                    arbos_state::L2_PER_BLOCK_GAS_LIMIT_OFFSET,
                    U256::from(call.limit),
                )?;
                Self::finish_write::<IArbOwner::setMaxBlockGasLimitCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setL2GasPricingInertia(call) => {
                if call.sec == 0 {
                    return empty_revert(gas_limit, storage.gas_used);
                }
                let l2_key = storage.l2_key();
                storage.write(
                    &l2_key,
                    arbos_state::L2_PRICING_INERTIA_OFFSET,
                    U256::from(call.sec),
                )?;
                Self::finish_write::<IArbOwner::setL2GasPricingInertiaCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setL2GasBacklogTolerance(call) => {
                let l2_key = storage.l2_key();
                storage.write(
                    &l2_key,
                    arbos_state::L2_BACKLOG_TOLERANCE_OFFSET,
                    U256::from(call.sec),
                )?;
                Self::finish_write::<IArbOwner::setL2GasBacklogToleranceCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setGasBacklog(call) => {
                let l2_key = storage.l2_key();
                storage.write(
                    &l2_key,
                    arbos_state::L2_GAS_BACKLOG_OFFSET,
                    U256::from(call.backlog),
                )?;
                Self::finish_write::<IArbOwner::setGasBacklogCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setGasPricingConstraints(call) => {
                storage.clear_gas_pricing_constraints()?;
                if current_arbos_version >= ARBOS_VERSION_MULTI_CONSTRAINT_FIX
                    && current_arbos_version < ARBOS_VERSION_MULTI_GAS_CONSTRAINTS
                    && call.constraints.len() > MAX_GAS_PRICING_CONSTRAINTS
                {
                    return Err(PrecompileError::other(format!(
                        "too many constraints. Max: {MAX_GAS_PRICING_CONSTRAINTS}"
                    )));
                }

                for constraint in &call.constraints {
                    if constraint[0] == 0 || constraint[1] == 0 {
                        return Err(PrecompileError::other(format!(
                            "invalid constraint with target {} and adjustment window {}",
                            constraint[0], constraint[1]
                        )));
                    }
                    storage.push_gas_pricing_constraint(*constraint)?;
                }
                Self::finish_write::<IArbOwner::setGasPricingConstraintsCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setMultiGasPricingConstraints(call) => {
                storage.clear_multi_gas_pricing_constraints()?;
                let mut constraints = Vec::with_capacity(call.constraints.len());
                for constraint in &call.constraints {
                    let constraint = Self::multi_gas_constraint(constraint)?;
                    storage.push_multi_gas_pricing_constraint(&constraint)?;
                    constraints.push(constraint);
                }
                Self::validate_multi_gas_exponents(&constraints)?;
                Self::finish_write::<IArbOwner::setMultiGasPricingConstraintsCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setL1PricingEquilibrationUnits(call) => {
                let l1_key = storage.l1_key();
                storage.write(
                    &l1_key,
                    arbos_state::L1_EQUILIBRATION_UNITS_OFFSET,
                    call.equilibrationUnits,
                )?;
                Self::finish_write::<IArbOwner::setL1PricingEquilibrationUnitsCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setL1PricingInertia(call) => {
                let l1_key = storage.l1_key();
                storage.write(
                    &l1_key,
                    arbos_state::L1_INERTIA_OFFSET,
                    U256::from(call.inertia),
                )?;
                Self::finish_write::<IArbOwner::setL1PricingInertiaCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setL1PricingRewardRecipient(call) => {
                let l1_key = storage.l1_key();
                storage.write_address(
                    &l1_key,
                    arbos_state::L1_PAY_REWARDS_TO_OFFSET,
                    call.recipient,
                )?;
                Self::finish_write::<IArbOwner::setL1PricingRewardRecipientCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setL1PricingRewardRate(call) => {
                let l1_key = storage.l1_key();
                storage.write(
                    &l1_key,
                    arbos_state::L1_PER_UNIT_REWARD_OFFSET,
                    U256::from(call.weiPerUnit),
                )?;
                Self::finish_write::<IArbOwner::setL1PricingRewardRateCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setL1PricePerUnit(call) => {
                let l1_key = storage.l1_key();
                storage.write(
                    &l1_key,
                    arbos_state::L1_PRICE_PER_UNIT_OFFSET,
                    call.pricePerUnit,
                )?;
                Self::finish_write::<IArbOwner::setL1PricePerUnitCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setParentGasFloorPerToken(call) => {
                let l1_key = storage.l1_key();
                storage.write(
                    &l1_key,
                    arbos_state::L1_GAS_FLOOR_PER_TOKEN_OFFSET,
                    U256::from(call.floorPerToken),
                )?;
                Self::finish_write::<IArbOwner::setParentGasFloorPerTokenCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setPerBatchGasCharge(call) => {
                let l1_key = storage.l1_key();
                storage.write(
                    &l1_key,
                    arbos_state::L1_PER_BATCH_GAS_COST_OFFSET,
                    U256::from(call.cost as u64),
                )?;
                Self::finish_write::<IArbOwner::setPerBatchGasChargeCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setBrotliCompressionLevel(call) => {
                if call.level > 11 {
                    return empty_revert(gas_limit, storage.gas_used);
                }
                storage.write(
                    &[],
                    arbos_state::BROTLI_COMPRESSION_LEVEL_OFFSET,
                    U256::from(call.level),
                )?;
                Self::finish_write::<IArbOwner::setBrotliCompressionLevelCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setAmortizedCostCapBips(call) => {
                let l1_key = storage.l1_key();
                storage.write(
                    &l1_key,
                    arbos_state::L1_AMORTIZED_COST_CAP_BIPS_OFFSET,
                    U256::from(call.cap),
                )?;
                Self::finish_write::<IArbOwner::setAmortizedCostCapBipsCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setInkPrice(call) => {
                if call.price == 0 || call.price > MAX_UINT24 {
                    return empty_revert(gas_limit, storage.gas_used);
                }
                let mut params = storage.stylus_params()?;
                params.ink_price = call.price;
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setInkPriceCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setWasmMaxStackDepth(call) => {
                let mut params = storage.stylus_params()?;
                params.max_stack_depth = call.depth;
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setWasmMaxStackDepthCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setWasmFreePages(call) => {
                let mut params = storage.stylus_params()?;
                params.free_pages = call.pages;
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setWasmFreePagesCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setWasmPageGas(call) => {
                let mut params = storage.stylus_params()?;
                params.page_gas = call.gas;
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setWasmPageGasCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setWasmPageLimit(call) => {
                let mut params = storage.stylus_params()?;
                params.page_limit = call.limit;
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setWasmPageLimitCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setWasmMaxSize(call) => {
                let mut params = storage.stylus_params()?;
                params.max_wasm_size = call.size;
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setWasmMaxSizeCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setWasmMinInitGas(call) => {
                let mut params = storage.stylus_params()?;
                params.min_init_gas = Self::saturating_u8(Self::div_ceil_u64(
                    u64::from(call.gas),
                    MIN_INIT_GAS_UNITS,
                ));
                params.min_cached_init_gas = Self::saturating_u8(Self::div_ceil_u64(
                    u64::from(call.cached),
                    MIN_CACHED_GAS_UNITS,
                ));
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setWasmMinInitGasCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setWasmInitCostScalar(call) => {
                let mut params = storage.stylus_params()?;
                params.init_cost_scalar =
                    Self::saturating_u8(Self::div_ceil_u64(call.percent, COST_SCALAR_PERCENT));
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setWasmInitCostScalarCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setWasmExpiryDays(call) => {
                let mut params = storage.stylus_params()?;
                params.expiry_days = call.days;
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setWasmExpiryDaysCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setWasmKeepaliveDays(call) => {
                let mut params = storage.stylus_params()?;
                params.keepalive_days = call.days;
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setWasmKeepaliveDaysCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setWasmBlockCacheSize(call) => {
                let mut params = storage.stylus_params()?;
                params.block_cache_size = call.count;
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setWasmBlockCacheSizeCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setMaxStylusContractFragments(call) => {
                let mut params = storage.stylus_params()?;
                params.max_fragment_count = call.maxFragments;
                storage.save_stylus_params(params)?;
                Self::finish_write::<IArbOwner::setMaxStylusContractFragmentsCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::releaseL1PricerSurplusFunds(call) => {
                let ret = storage.release_l1_pricer_surplus(call.maxWeiToRelease)?;
                Self::finish_write::<IArbOwner::releaseL1PricerSurplusFundsCall, DB>(
                    storage, gas_limit, data, caller, ret,
                )
            }
            IArbOwner::IArbOwnerCalls::setCalldataPriceIncrease(call) => {
                let features_key = arbos_state::child_key(&[], arbos_state::FEATURES_SUBSPACE);
                let current = storage.read(&features_key, 0)?;
                let next = if call.enable {
                    current | U256::from(1u8)
                } else {
                    current & !U256::from(1u8)
                };
                storage.write(&features_key, 0, next)?;
                Self::finish_write::<IArbOwner::setCalldataPriceIncreaseCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setCollectTips(call) => {
                storage.write(
                    &[],
                    arbos_state::COLLECT_TIPS_OFFSET,
                    U256::from(u8::from(call.collectTips)),
                )?;
                Self::finish_write::<IArbOwner::setCollectTipsCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::addWasmCacheManager(call) => {
                let managers_key = storage.wasm_cache_manager_key();
                storage.address_set_add(&managers_key, call.manager)?;
                Self::finish_write::<IArbOwner::addWasmCacheManagerCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::removeWasmCacheManager(call) => {
                let managers_key = storage.wasm_cache_manager_key();
                if !storage.address_set_contains(&managers_key, call.manager)? {
                    return empty_revert(gas_limit, storage.gas_used);
                }
                storage.address_set_remove(&managers_key, call.manager)?;
                Self::finish_write::<IArbOwner::removeWasmCacheManagerCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setWasmActivationGas(call) => {
                let activation_key = storage.wasm_activation_gas_key();
                storage.write(&activation_key, 0, U256::from(call.gas))?;
                Self::finish_write::<IArbOwner::setWasmActivationGasCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::setChainConfig(call) => {
                Self::set_chain_config(storage, &call.chainConfig, current_chain_config)?;
                Self::finish_write::<IArbOwner::setChainConfigCall, DB>(
                    storage,
                    gas_limit,
                    data,
                    caller,
                    ().into(),
                )
            }
            IArbOwner::IArbOwnerCalls::isChainOwner(_)
            | IArbOwner::IArbOwnerCalls::getAllChainOwners(_)
            | IArbOwner::IArbOwnerCalls::isNativeTokenOwner(_)
            | IArbOwner::IArbOwnerCalls::getAllNativeTokenOwners(_)
            | IArbOwner::IArbOwnerCalls::isTransactionFilterer(_)
            | IArbOwner::IArbOwnerCalls::getAllTransactionFilterers(_)
            | IArbOwner::IArbOwnerCalls::getFilteredFundsRecipient(_)
            | IArbOwner::IArbOwnerCalls::getNetworkFeeAccount(_)
            | IArbOwner::IArbOwnerCalls::getInfraFeeAccount(_) => {
                empty_revert(gas_limit, storage.gas_used)
            }
        };

        match result {
            Err(PrecompileError::Other(_)) => empty_revert(gas_limit, storage.gas_used),
            other => other,
        }
    }

    fn caller_is_owner<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
    ) -> Result<bool, PrecompileError> {
        let owners_key = storage.chain_owner_key();
        storage.address_set_contains(&owners_key, caller)
    }

    fn set_chain_config<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        chain_config: &str,
        current_chain_config: Option<&[u8]>,
    ) -> Result<(), PrecompileError> {
        Self::validate_chain_config(storage, chain_config, current_chain_config)?;
        let config_key = storage.chain_config_key();
        storage.write_bytes(&config_key, chain_config.as_bytes())
    }

    fn validate_chain_config<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        chain_config: &str,
        current_chain_config: Option<&[u8]>,
    ) -> Result<(), PrecompileError> {
        let new_config = NitroChainConfig::parse(chain_config.as_bytes())?;
        let current_chain_id = storage.context.cfg().chain_id;
        new_config.ensure_chain_id(current_chain_id)?;

        let config_key = storage.chain_config_key();
        let old_serialized_config = storage.read_bytes(&config_key)?;
        if old_serialized_config.as_ref() == chain_config.as_bytes() {
            return Err(PrecompileError::other("chain config is already set"));
        }
        if !old_serialized_config.is_empty() {
            let old_config = NitroChainConfig::parse(old_serialized_config.as_ref())?;
            old_config.check_compatible(
                &new_config,
                storage.current_l2_block_number_u64(),
                storage.context.block().timestamp().to::<u64>(),
            )?;
        }
        let current_chain_config = current_chain_config
            .ok_or_else(|| PrecompileError::other("chain config missing current EVM config"))?;
        let current_config = NitroChainConfig::parse(current_chain_config)?;
        current_config.check_compatible(
            &new_config,
            storage.current_l2_block_number_u64(),
            storage.context.block().timestamp().to::<u64>(),
        )?;

        Ok(())
    }

    fn method_is_available(current_arbos_version: u64, call: &IArbOwner::IArbOwnerCalls) -> bool {
        current_arbos_version >= Self::required_arbos_version(call)
    }

    fn required_arbos_version(call: &IArbOwner::IArbOwnerCalls) -> u64 {
        match call {
            IArbOwner::IArbOwnerCalls::getInfraFeeAccount(_)
            | IArbOwner::IArbOwnerCalls::setInfraFeeAccount(_) => ARBOS_VERSION_5,
            IArbOwner::IArbOwnerCalls::releaseL1PricerSurplusFunds(_) => ARBOS_VERSION_10,
            IArbOwner::IArbOwnerCalls::setChainConfig(_) => ARBOS_VERSION_11,
            IArbOwner::IArbOwnerCalls::setBrotliCompressionLevel(_) => ARBOS_VERSION_20,
            IArbOwner::IArbOwnerCalls::setInkPrice(_)
            | IArbOwner::IArbOwnerCalls::setWasmMaxStackDepth(_)
            | IArbOwner::IArbOwnerCalls::setWasmFreePages(_)
            | IArbOwner::IArbOwnerCalls::setWasmPageGas(_)
            | IArbOwner::IArbOwnerCalls::setWasmPageLimit(_)
            | IArbOwner::IArbOwnerCalls::setWasmMinInitGas(_)
            | IArbOwner::IArbOwnerCalls::setWasmInitCostScalar(_)
            | IArbOwner::IArbOwnerCalls::setWasmExpiryDays(_)
            | IArbOwner::IArbOwnerCalls::setWasmKeepaliveDays(_)
            | IArbOwner::IArbOwnerCalls::setWasmBlockCacheSize(_)
            | IArbOwner::IArbOwnerCalls::addWasmCacheManager(_)
            | IArbOwner::IArbOwnerCalls::removeWasmCacheManager(_) => ARBOS_VERSION_STYLUS,
            IArbOwner::IArbOwnerCalls::setCalldataPriceIncrease(_)
            | IArbOwner::IArbOwnerCalls::setWasmMaxSize(_) => ARBOS_VERSION_40,
            IArbOwner::IArbOwnerCalls::setNativeTokenManagementFrom(_)
            | IArbOwner::IArbOwnerCalls::addNativeTokenOwner(_)
            | IArbOwner::IArbOwnerCalls::removeNativeTokenOwner(_)
            | IArbOwner::IArbOwnerCalls::isNativeTokenOwner(_)
            | IArbOwner::IArbOwnerCalls::getAllNativeTokenOwners(_) => ARBOS_VERSION_41,
            IArbOwner::IArbOwnerCalls::setGasPricingConstraints(_)
            | IArbOwner::IArbOwnerCalls::setGasBacklog(_)
            | IArbOwner::IArbOwnerCalls::setParentGasFloorPerToken(_)
            | IArbOwner::IArbOwnerCalls::setMaxBlockGasLimit(_) => ARBOS_VERSION_50,
            IArbOwner::IArbOwnerCalls::setWasmActivationGas(_) => ARBOS_VERSION_59,
            IArbOwner::IArbOwnerCalls::setMultiGasPricingConstraints(_)
            | IArbOwner::IArbOwnerCalls::setMaxStylusContractFragments(_)
            | IArbOwner::IArbOwnerCalls::setCollectTips(_)
            | IArbOwner::IArbOwnerCalls::setTransactionFilteringFrom(_)
            | IArbOwner::IArbOwnerCalls::addTransactionFilterer(_)
            | IArbOwner::IArbOwnerCalls::removeTransactionFilterer(_)
            | IArbOwner::IArbOwnerCalls::isTransactionFilterer(_)
            | IArbOwner::IArbOwnerCalls::getAllTransactionFilterers(_)
            | IArbOwner::IArbOwnerCalls::setFilteredFundsRecipient(_)
            | IArbOwner::IArbOwnerCalls::getFilteredFundsRecipient(_) => {
                ARBOS_VERSION_MULTI_GAS_CONSTRAINTS
            }
            _ => 0,
        }
    }

    fn multi_gas_constraint(
        constraint: &super::abi::MultiGasConstraint,
    ) -> Result<MultiGasPricingConstraint, PrecompileError> {
        if constraint.targetPerSec == 0 || constraint.adjustmentWindowSecs == 0 {
            return Err(PrecompileError::other(format!(
                "invalid constraint: target={} adjustmentWindow={}",
                constraint.targetPerSec, constraint.adjustmentWindowSecs
            )));
        }

        let mut resources = [0u64; NUM_RESOURCE_KIND];
        for weighted in &constraint.resources {
            let resource = usize::from(weighted.resource);
            if resource <= RESOURCE_KIND_UNKNOWN || resource >= NUM_RESOURCE_KIND {
                return Err(PrecompileError::other(format!(
                    "invalid resource id: {}",
                    weighted.resource
                )));
            }
            resources[resource] = weighted.weight;
        }

        Ok(MultiGasPricingConstraint {
            resources,
            adjustment_window_secs: constraint.adjustmentWindowSecs,
            target_per_sec: constraint.targetPerSec,
            backlog: constraint.backlog,
        })
    }

    fn validate_multi_gas_exponents(
        constraints: &[MultiGasPricingConstraint],
    ) -> Result<(), PrecompileError> {
        let mut exponents = [0i64; NUM_RESOURCE_KIND];
        for constraint in constraints {
            if constraint.backlog == 0 {
                continue;
            }

            let max_weight = constraint.max_weight();
            if max_weight == 0 {
                continue;
            }

            let divisor = Self::saturating_i64_from_u64(Self::saturating_u64_mul(
                u64::from(constraint.adjustment_window_secs),
                Self::saturating_u64_mul(constraint.target_per_sec, max_weight),
            ));
            if divisor == 0 {
                continue;
            }

            for (resource, weight) in constraint.resources.iter().copied().enumerate() {
                if resource == RESOURCE_KIND_SINGLE_DIM || weight == 0 {
                    continue;
                }

                let dividend =
                    Self::natural_to_bips(Self::saturating_u64_mul(constraint.backlog, weight));
                let exponent = dividend / divisor;
                exponents[resource] = exponents[resource].saturating_add(exponent);
            }
        }

        for exponent in exponents {
            if exponent > MAX_PRICING_EXPONENT_BIPS {
                return Err(PrecompileError::other(format!(
                    "calculated exponent {exponent} exceeds maximum allowed {MAX_PRICING_EXPONENT_BIPS}"
                )));
            }
        }
        Ok(())
    }

    fn set_feature_from_time<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        offset: u64,
        timestamp: u64,
    ) -> Result<(), PrecompileError> {
        if timestamp == 0 {
            return storage.write(&[], offset, U256::ZERO);
        }

        let now = storage.context.block().timestamp().to::<u64>();
        let stored = storage.root(offset)?.to::<u64>();
        let earliest = now.saturating_add(FEATURE_ENABLE_DELAY);
        if (stored == 0 && timestamp < earliest) || (stored > earliest && timestamp < earliest) {
            return Err(PrecompileError::other(
                "feature must be enabled at least 7 days in the future",
            ));
        }
        if stored > now && stored <= earliest && timestamp < stored {
            return Err(PrecompileError::other(
                "feature cannot be updated to a time earlier than the current scheduled enable time",
            ));
        }

        storage.write(&[], offset, U256::from(timestamp))
    }

    fn ensure_feature_enabled<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        offset: u64,
        reason: &'static str,
    ) -> Result<(), PrecompileError> {
        let enabled_time = storage.root(offset)?.to::<u64>();
        let now = storage.context.block().timestamp().to::<u64>();
        if enabled_time == 0 || enabled_time > now {
            return Err(PrecompileError::other(reason));
        }
        Ok(())
    }

    fn finish_write<Call: SolCall, DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        gas_limit: u64,
        data: &[u8],
        caller: Address,
        ret: Call::Return,
    ) -> PrecompileResult {
        let output = finish_call::<Call>(gas_limit, storage.gas_used, ret)?;
        Self::emit_owner_acts(storage, data, caller);
        Ok(output)
    }

    fn finish_view<Call: SolCall, DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        gas_limit: u64,
        data: &[u8],
        caller: Address,
        is_static: bool,
        current_arbos_version: u64,
        ret: Call::Return,
    ) -> PrecompileResult {
        let output = finish_call::<Call>(gas_limit, storage.gas_used, ret)?;
        if !is_static || current_arbos_version < ARBOS_VERSION_11 {
            Self::emit_owner_acts(storage, data, caller);
        }
        Ok(output)
    }

    fn finish_owner_call(gas_limit: u64, result: PrecompileResult) -> PrecompileResult {
        match result {
            Ok(mut output) => {
                output.gas_used = 0;
                Ok(output)
            }
            Err(PrecompileError::Fatal(err)) => Err(PrecompileError::Fatal(err)),
            Err(_) => empty_revert(gas_limit, 0),
        }
    }

    fn div_ceil_u64(lhs: u64, rhs: u64) -> u64 {
        lhs / rhs + u64::from(lhs % rhs != 0)
    }

    fn saturating_u8(value: u64) -> u8 {
        u8::try_from(value).unwrap_or(u8::MAX)
    }

    fn saturating_u64_mul(lhs: u64, rhs: u64) -> u64 {
        lhs.checked_mul(rhs).unwrap_or(u64::MAX)
    }

    fn saturating_i64_from_u64(value: u64) -> i64 {
        i64::try_from(value).unwrap_or(i64::MAX)
    }

    fn natural_to_bips(value: u64) -> i64 {
        Self::saturating_i64_from_u64(value)
            .checked_mul(ONE_IN_BIPS)
            .unwrap_or(i64::MAX)
    }

    fn emit_owner_acts<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        data: &[u8],
        caller: Address,
    ) {
        let mut selector_topic = [0u8; 32];
        if let Some(selector) = data.get(..4) {
            selector_topic[..4].copy_from_slice(selector);
        }
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_OWNER_ADDRESS,
            vec![
                keccak256("OwnerActs(bytes4,address,bytes)"),
                B256::from(selector_topic),
                topic_address(caller),
            ],
            Bytes::from((Bytes::copy_from_slice(data),).abi_encode()),
        ));
    }

    fn emit_address<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        event: &'static str,
        address: Address,
    ) {
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_OWNER_ADDRESS,
            vec![keccak256(event), topic_address(address)],
            Bytes::new(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BASE_PRECOMPILE_GAS, STORAGE_READ_GAS};
    use super::*;
    use crate::arbitrum::context::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::{Context, MainContext};

    const WORD_COPY_GAS: u64 = 3;

    fn context() -> ArbitrumContext<CacheDB<EmptyDB>> {
        context_with_block(BlockEnv::default())
    }

    fn context_with_block(block: BlockEnv) -> ArbitrumContext<CacheDB<EmptyDB>> {
        let db = CacheDB::new(EmptyDB::default());
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(block)
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(db)
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");
        context
    }

    fn add_chain_owner<DB: Database>(context: &mut ArbitrumContext<DB>, owner: Address) {
        let mut storage = ArbStorage::new_with_initial_gas(context, u64::MAX, 0);
        let owners_key = storage.chain_owner_key();
        storage
            .address_set_add(&owners_key, owner)
            .expect("add chain owner");
    }

    #[test]
    fn method_versions_match_nitro_registration() {
        let address = Address::ZERO;
        let cases = [
            (
                IArbOwner::IArbOwnerCalls::getInfraFeeAccount(IArbOwner::getInfraFeeAccountCall {}),
                ARBOS_VERSION_5,
            ),
            (
                IArbOwner::IArbOwnerCalls::setInfraFeeAccount(IArbOwner::setInfraFeeAccountCall {
                    newInfraFeeAccount: address,
                }),
                ARBOS_VERSION_5,
            ),
            (
                IArbOwner::IArbOwnerCalls::releaseL1PricerSurplusFunds(
                    IArbOwner::releaseL1PricerSurplusFundsCall {
                        maxWeiToRelease: U256::ZERO,
                    },
                ),
                ARBOS_VERSION_10,
            ),
            (
                IArbOwner::IArbOwnerCalls::setChainConfig(IArbOwner::setChainConfigCall {
                    chainConfig: String::new(),
                }),
                ARBOS_VERSION_11,
            ),
            (
                IArbOwner::IArbOwnerCalls::setBrotliCompressionLevel(
                    IArbOwner::setBrotliCompressionLevelCall { level: 0 },
                ),
                ARBOS_VERSION_20,
            ),
            (
                IArbOwner::IArbOwnerCalls::setInkPrice(IArbOwner::setInkPriceCall { price: 1 }),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setWasmMaxStackDepth(
                    IArbOwner::setWasmMaxStackDepthCall { depth: 0 },
                ),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setWasmFreePages(IArbOwner::setWasmFreePagesCall {
                    pages: 0,
                }),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setWasmPageGas(IArbOwner::setWasmPageGasCall { gas: 0 }),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setWasmPageLimit(IArbOwner::setWasmPageLimitCall {
                    limit: 0,
                }),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setWasmMinInitGas(IArbOwner::setWasmMinInitGasCall {
                    gas: 0,
                    cached: 0,
                }),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setWasmInitCostScalar(
                    IArbOwner::setWasmInitCostScalarCall { percent: 0 },
                ),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setWasmExpiryDays(IArbOwner::setWasmExpiryDaysCall {
                    days: 0,
                }),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setWasmKeepaliveDays(
                    IArbOwner::setWasmKeepaliveDaysCall { days: 0 },
                ),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setWasmBlockCacheSize(
                    IArbOwner::setWasmBlockCacheSizeCall { count: 0 },
                ),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::addWasmCacheManager(
                    IArbOwner::addWasmCacheManagerCall { manager: address },
                ),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::removeWasmCacheManager(
                    IArbOwner::removeWasmCacheManagerCall { manager: address },
                ),
                ARBOS_VERSION_STYLUS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setCalldataPriceIncrease(
                    IArbOwner::setCalldataPriceIncreaseCall { enable: true },
                ),
                ARBOS_VERSION_40,
            ),
            (
                IArbOwner::IArbOwnerCalls::setWasmMaxSize(IArbOwner::setWasmMaxSizeCall {
                    size: 0,
                }),
                ARBOS_VERSION_40,
            ),
            (
                IArbOwner::IArbOwnerCalls::setNativeTokenManagementFrom(
                    IArbOwner::setNativeTokenManagementFromCall { timestamp: 0 },
                ),
                ARBOS_VERSION_41,
            ),
            (
                IArbOwner::IArbOwnerCalls::addNativeTokenOwner(
                    IArbOwner::addNativeTokenOwnerCall { newOwner: address },
                ),
                ARBOS_VERSION_41,
            ),
            (
                IArbOwner::IArbOwnerCalls::removeNativeTokenOwner(
                    IArbOwner::removeNativeTokenOwnerCall {
                        ownerToRemove: address,
                    },
                ),
                ARBOS_VERSION_41,
            ),
            (
                IArbOwner::IArbOwnerCalls::isNativeTokenOwner(IArbOwner::isNativeTokenOwnerCall {
                    addr: address,
                }),
                ARBOS_VERSION_41,
            ),
            (
                IArbOwner::IArbOwnerCalls::getAllNativeTokenOwners(
                    IArbOwner::getAllNativeTokenOwnersCall {},
                ),
                ARBOS_VERSION_41,
            ),
            (
                IArbOwner::IArbOwnerCalls::setGasPricingConstraints(
                    IArbOwner::setGasPricingConstraintsCall {
                        constraints: Vec::new(),
                    },
                ),
                ARBOS_VERSION_50,
            ),
            (
                IArbOwner::IArbOwnerCalls::setGasBacklog(IArbOwner::setGasBacklogCall {
                    backlog: 0,
                }),
                ARBOS_VERSION_50,
            ),
            (
                IArbOwner::IArbOwnerCalls::setParentGasFloorPerToken(
                    IArbOwner::setParentGasFloorPerTokenCall { floorPerToken: 0 },
                ),
                ARBOS_VERSION_50,
            ),
            (
                IArbOwner::IArbOwnerCalls::setMaxBlockGasLimit(
                    IArbOwner::setMaxBlockGasLimitCall { limit: 0 },
                ),
                ARBOS_VERSION_50,
            ),
            (
                IArbOwner::IArbOwnerCalls::setWasmActivationGas(
                    IArbOwner::setWasmActivationGasCall { gas: 0 },
                ),
                ARBOS_VERSION_59,
            ),
            (
                IArbOwner::IArbOwnerCalls::setMultiGasPricingConstraints(
                    IArbOwner::setMultiGasPricingConstraintsCall {
                        constraints: Vec::new(),
                    },
                ),
                ARBOS_VERSION_MULTI_GAS_CONSTRAINTS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setMaxStylusContractFragments(
                    IArbOwner::setMaxStylusContractFragmentsCall { maxFragments: 0 },
                ),
                ARBOS_VERSION_MULTI_GAS_CONSTRAINTS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setCollectTips(IArbOwner::setCollectTipsCall {
                    collectTips: true,
                }),
                ARBOS_VERSION_MULTI_GAS_CONSTRAINTS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setTransactionFilteringFrom(
                    IArbOwner::setTransactionFilteringFromCall { timestamp: 0 },
                ),
                ARBOS_VERSION_MULTI_GAS_CONSTRAINTS,
            ),
            (
                IArbOwner::IArbOwnerCalls::addTransactionFilterer(
                    IArbOwner::addTransactionFiltererCall { filterer: address },
                ),
                ARBOS_VERSION_MULTI_GAS_CONSTRAINTS,
            ),
            (
                IArbOwner::IArbOwnerCalls::removeTransactionFilterer(
                    IArbOwner::removeTransactionFiltererCall { filterer: address },
                ),
                ARBOS_VERSION_MULTI_GAS_CONSTRAINTS,
            ),
            (
                IArbOwner::IArbOwnerCalls::isTransactionFilterer(
                    IArbOwner::isTransactionFiltererCall { filterer: address },
                ),
                ARBOS_VERSION_MULTI_GAS_CONSTRAINTS,
            ),
            (
                IArbOwner::IArbOwnerCalls::getAllTransactionFilterers(
                    IArbOwner::getAllTransactionFilterersCall {},
                ),
                ARBOS_VERSION_MULTI_GAS_CONSTRAINTS,
            ),
            (
                IArbOwner::IArbOwnerCalls::setFilteredFundsRecipient(
                    IArbOwner::setFilteredFundsRecipientCall {
                        newRecipient: address,
                    },
                ),
                ARBOS_VERSION_MULTI_GAS_CONSTRAINTS,
            ),
            (
                IArbOwner::IArbOwnerCalls::getFilteredFundsRecipient(
                    IArbOwner::getFilteredFundsRecipientCall {},
                ),
                ARBOS_VERSION_MULTI_GAS_CONSTRAINTS,
            ),
        ];

        for (call, version) in cases {
            assert_eq!(ArbOwner::required_arbos_version(&call), version);
        }

        assert_eq!(
            ArbOwner::required_arbos_version(&IArbOwner::IArbOwnerCalls::addChainOwner(
                IArbOwner::addChainOwnerCall { newOwner: address }
            )),
            0
        );
    }

    #[test]
    fn set_wasm_min_init_gas_selector_matches_nitro_solidity_abi() {
        assert_eq!(
            <IArbOwner::setWasmMinInitGasCall as SolCall>::SELECTOR,
            [0x82, 0x93, 0x40, 0x5e]
        );
    }

    fn input<'a, DB: Database>(
        data: &'a [u8],
        caller: Address,
        is_static: bool,
        current_arbos_version: u64,
        context: &'a mut ArbitrumContext<DB>,
    ) -> ArbPrecompileInput<'a, ArbitrumContext<DB>> {
        ArbPrecompileInput {
            data,
            gas: 10_000_000,
            caller,
            value: U256::ZERO,
            is_static,
            is_valid_call_context: true,
            current_arbos_version,
            current_tx_l1_gas_fees: U256::ZERO,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context,
        }
    }

    #[test]
    fn unauthorized_call_checks_owner_before_decode() {
        let caller = Address::from([1; 20]);
        let mut context = context();

        let output = ArbOwner::run(input(&[], caller, false, 60, &mut context))
            .expect("unauthorized call should revert");

        assert!(output.reverted);
        assert!(output.gas_used < 10_000_000);
    }

    #[test]
    fn owner_set_wasm_min_init_gas_uses_nitro_scaling() {
        let caller = Address::from([8; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            storage
                .write(&[], arbos_state::ARBOS_VERSION_OFFSET, U256::from(60))
                .expect("write ArbOS version");
        }

        let data = IArbOwner::setWasmMinInitGasCall {
            gas: 129,
            cached: u16::MAX,
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("setWasmMinInitGas should succeed");

        assert!(!output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let params = storage.stylus_params().expect("read stylus params");
        assert_eq!(params.min_init_gas, 2);
        assert_eq!(params.min_cached_init_gas, u8::MAX);
    }

    #[test]
    fn owner_set_wasm_min_init_gas_rejects_go_handler_signature_selector() {
        let caller = Address::from([9; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let go_handler_signature_selector = [0x80, 0xa3, 0xa5, 0xe4];
        let output = ArbOwner::run(input(
            &go_handler_signature_selector,
            caller,
            false,
            60,
            &mut context,
        ))
        .expect("Go handler signature selector should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);
        assert!(output.bytes.is_empty());
    }

    #[test]
    fn owner_set_minimum_l2_base_fee_allows_zero_on_mutating_call() {
        let caller = Address::from([10; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            let l2_key = storage.l2_key();
            storage
                .write(
                    &l2_key,
                    arbos_state::L2_MIN_BASE_FEE_WEI_OFFSET,
                    U256::from(1),
                )
                .expect("seed minimum base fee");
        }

        let data = IArbOwner::setMinimumL2BaseFeeCall {
            priceInWei: U256::ZERO,
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("setMinimumL2BaseFee should succeed");

        assert!(!output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let l2_key = storage.l2_key();
        let minimum_base_fee = storage
            .read(&l2_key, arbos_state::L2_MIN_BASE_FEE_WEI_OFFSET)
            .expect("read minimum base fee");
        assert_eq!(minimum_base_fee, U256::ZERO);
    }

    #[test]
    fn owner_set_minimum_l2_base_fee_reverts_on_staticcall() {
        let caller = Address::from([10; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            let l2_key = storage.l2_key();
            storage
                .write(
                    &l2_key,
                    arbos_state::L2_MIN_BASE_FEE_WEI_OFFSET,
                    U256::from(1),
                )
                .expect("seed minimum base fee");
        }

        let data = IArbOwner::setMinimumL2BaseFeeCall {
            priceInWei: U256::ZERO,
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, true, 60, &mut context))
            .expect("setMinimumL2BaseFee staticcall should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);
        assert!(context.journal_mut().take_logs().is_empty());

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let l2_key = storage.l2_key();
        let minimum_base_fee = storage
            .read(&l2_key, arbos_state::L2_MIN_BASE_FEE_WEI_OFFSET)
            .expect("read minimum base fee");
        assert_eq!(minimum_base_fee, U256::from(1));
    }

    #[test]
    fn owner_set_chain_config_writes_valid_config() {
        let caller = Address::from([11; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let chain_config = r#"{"chainId":1,"homesteadBlock":0}"#;
        let data = IArbOwner::setChainConfigCall {
            chainConfig: chain_config.to_string(),
        }
        .abi_encode();
        let mut input = input(&data, caller, false, 60, &mut context);
        input.current_chain_config = Some(chain_config.as_bytes());
        let output = ArbOwner::run(input).expect("setChainConfig should succeed");

        assert!(!output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert_eq!(stored.as_ref(), chain_config.as_bytes());
    }

    #[test]
    fn owner_set_chain_config_accepts_big_int_fields() {
        let caller = Address::from([25; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let chain_config = r#"{"chainId":1,"eip150Block":18446744073709551616,"terminalTotalDifficulty":58750000000000000000000}"#;
        let data = IArbOwner::setChainConfigCall {
            chainConfig: chain_config.to_string(),
        }
        .abi_encode();
        let mut input = input(&data, caller, false, 60, &mut context);
        input.current_chain_config = Some(chain_config.as_bytes());
        let output = ArbOwner::run(input).expect("setChainConfig with big integers should succeed");

        assert!(!output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert_eq!(stored.as_ref(), chain_config.as_bytes());
    }

    #[test]
    fn owner_set_chain_config_rejects_invalid_json() {
        let caller = Address::from([12; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::setChainConfigCall {
            chainConfig: "not json".to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("invalid chain config should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert!(stored.is_empty());
    }

    #[test]
    fn owner_set_chain_config_rejects_missing_chain_id() {
        let caller = Address::from([13; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"homesteadBlock":0}"#.to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("missing chainId should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert!(stored.is_empty());
    }

    #[test]
    fn owner_set_chain_config_rejects_chain_id_mismatch() {
        let caller = Address::from([14; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"chainId":42161}"#.to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("chainId mismatch should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert!(stored.is_empty());
    }

    #[test]
    fn owner_set_chain_config_rejects_missing_current_chain_config() {
        let caller = Address::from([24; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"chainId":1}"#.to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("missing current chain config should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert!(stored.is_empty());
    }

    #[test]
    fn owner_set_chain_config_rejects_string_chain_id() {
        let caller = Address::from([19; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"chainId":"1"}"#.to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("string chainId should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert!(stored.is_empty());
    }

    #[test]
    fn owner_set_chain_config_rejects_invalid_big_int_field() {
        let caller = Address::from([26; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"chainId":1,"terminalTotalDifficulty":1.5}"#.to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("invalid big integer field should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert!(stored.is_empty());
    }

    #[test]
    fn owner_set_chain_config_rejects_invalid_alias_field() {
        let caller = Address::from([27; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"chainId":1,"ChainID":"bad"}"#.to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("invalid alias field should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert!(stored.is_empty());
    }

    #[test]
    fn owner_set_chain_config_rejects_invalid_known_top_level_field() {
        let caller = Address::from([21; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"chainId":1,"ethash":1}"#.to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("invalid top-level chain config field should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert!(stored.is_empty());
    }

    #[test]
    fn owner_set_chain_config_rejects_invalid_blob_schedule_field() {
        let caller = Address::from([22; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"chainId":1,"blobSchedule":{"cancun":{"baseFeeUpdateFraction":"1"}}}"#
                .to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("invalid blob schedule field should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert!(stored.is_empty());
    }

    #[test]
    fn owner_set_chain_config_rejects_invalid_arbitrum_field() {
        let caller = Address::from([23; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"chainId":1,"arbitrum":{"InitialArbOSVersion":"1"}}"#.to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("invalid arbitrum chain config field should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert!(stored.is_empty());
    }

    #[test]
    fn owner_set_chain_config_rejects_same_config() {
        let caller = Address::from([15; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let chain_config = r#"{"chainId":1}"#;
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            let config_key = storage.chain_config_key();
            storage
                .write_bytes(&config_key, chain_config.as_bytes())
                .expect("seed chain config");
        }

        let data = IArbOwner::setChainConfigCall {
            chainConfig: chain_config.to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("same chain config should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert_eq!(stored.as_ref(), chain_config.as_bytes());
    }

    #[test]
    fn owner_set_chain_config_rejects_incompatible_old_block_fork() {
        let caller = Address::from([16; 20]);
        let mut context = context_with_block(BlockEnv {
            number: U256::from(25),
            ..Default::default()
        });
        add_chain_owner(&mut context, caller);

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            let config_key = storage.chain_config_key();
            storage
                .write_bytes(&config_key, br#"{"chainId":1,"eip150Block":10}"#)
                .expect("seed old chain config");
        }

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"chainId":1,"eip150Block":20}"#.to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("incompatible chain config should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert_eq!(stored.as_ref(), br#"{"chainId":1,"eip150Block":10}"#);
    }

    #[test]
    fn owner_set_chain_config_rejects_incompatible_current_chain_config() {
        let caller = Address::from([20; 20]);
        let mut context = context_with_block(BlockEnv {
            number: U256::from(25),
            ..Default::default()
        });
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"chainId":1,"eip150Block":20}"#.to_string(),
        }
        .abi_encode();
        let mut input = input(&data, caller, false, 60, &mut context);
        input.current_chain_config = Some(br#"{"chainId":1,"eip150Block":10}"#);
        let output =
            ArbOwner::run(input).expect("current chain config incompatibility should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert!(stored.is_empty());
    }

    #[test]
    fn owner_set_chain_config_allows_future_block_fork_change() {
        let caller = Address::from([17; 20]);
        let mut context = context_with_block(BlockEnv {
            number: U256::from(9),
            ..Default::default()
        });
        add_chain_owner(&mut context, caller);

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            let config_key = storage.chain_config_key();
            storage
                .write_bytes(&config_key, br#"{"chainId":1,"eip150Block":10}"#)
                .expect("seed old chain config");
        }

        let chain_config = r#"{"chainId":1,"eip150Block":20}"#;
        let data = IArbOwner::setChainConfigCall {
            chainConfig: chain_config.to_string(),
        }
        .abi_encode();
        let mut input = input(&data, caller, false, 60, &mut context);
        input.current_chain_config = Some(br#"{"chainId":1,"eip150Block":10}"#);
        let output = ArbOwner::run(input).expect("future fork change should succeed");

        assert!(!output.reverted);
        assert_eq!(output.gas_used, 0);

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let config_key = storage.chain_config_key();
        let stored = storage
            .read_bytes(&config_key)
            .expect("read stored chain config");
        assert_eq!(stored.as_ref(), chain_config.as_bytes());
    }

    #[test]
    fn owner_set_chain_config_rejects_incompatible_arbitrum_params() {
        let caller = Address::from([18; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
            let config_key = storage.chain_config_key();
            storage
                .write_bytes(
                    &config_key,
                    br#"{"chainId":1,"arbitrum":{"EnableArbOS":true,"GenesisBlockNum":0,"MaxUncompressedBatchSize":100}}"#,
                )
                .expect("seed old chain config");
        }

        let data = IArbOwner::setChainConfigCall {
            chainConfig: r#"{"chainId":1,"arbitrum":{"EnableArbOS":true,"GenesisBlockNum":0,"MaxUncompressedBatchSize":101}}"#.to_string(),
        }
        .abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("incompatible arbitrum params should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);
    }

    #[test]
    fn owner_view_call_is_free_and_emits_owner_acts_on_call() {
        let caller = Address::from([2; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::isChainOwnerCall { addr: caller }.abi_encode();
        let output = ArbOwner::run(input(&data, caller, false, 60, &mut context))
            .expect("owner call should succeed");

        assert!(!output.reverted);
        assert_eq!(output.gas_used, 0);
        assert_eq!(U256::from_be_slice(output.bytes.as_ref()), U256::from(1));

        let logs = context.journal_mut().take_logs();
        assert!(logs.iter().any(|log| {
            log.address == ARB_OWNER_ADDRESS
                && log.data.topics().first() == Some(&keccak256("OwnerActs(bytes4,address,bytes)"))
        }));
    }

    #[test]
    fn owner_static_view_call_is_free_without_owner_acts_after_arbos_11() {
        let caller = Address::from([3; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::isChainOwnerCall { addr: caller }.abi_encode();
        let output = ArbOwner::run(input(&data, caller, true, 60, &mut context))
            .expect("owner static call should succeed");

        assert!(!output.reverted);
        assert_eq!(output.gas_used, 0);

        let logs = context.journal_mut().take_logs();
        assert!(!logs.iter().any(|log| {
            log.address == ARB_OWNER_ADDRESS
                && log.data.topics().first() == Some(&keccak256("OwnerActs(bytes4,address,bytes)"))
        }));
    }

    #[test]
    fn owner_invalid_context_reverts_after_owner_check_without_charging_gas() {
        let caller = Address::from([4; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::isChainOwnerCall { addr: caller }.abi_encode();
        let mut input = input(&data, caller, false, 60, &mut context);
        input.is_valid_call_context = false;
        let output = ArbOwner::run(input).expect("invalid context should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);
    }

    #[test]
    fn owner_nonpayable_value_reverts_after_owner_check_without_charging_gas() {
        let caller = Address::from([5; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::isChainOwnerCall { addr: caller }.abi_encode();
        let mut input = input(&data, caller, false, 60, &mut context);
        input.value = U256::from(1);
        let output = ArbOwner::run(input).expect("nonpayable value should revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);
    }

    #[test]
    fn owner_unknown_selector_reverts_after_owner_check_without_charging_gas() {
        let caller = Address::from([6; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = [0xff, 0xff, 0xff, 0xff];
        let output =
            ArbOwner::run(input(&data, caller, false, 60, &mut context)).expect("unknown selector");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);
        assert!(output.bytes.is_empty());

        let logs = context.journal_mut().take_logs();
        assert!(!logs.iter().any(|log| {
            log.address == ARB_OWNER_ADDRESS
                && log.data.topics().first() == Some(&keccak256("OwnerActs(bytes4,address,bytes)"))
        }));
    }

    #[test]
    fn owner_post_auth_result_copy_oog_is_free_revert_without_owner_acts() {
        let caller = Address::from([7; 20]);
        let mut context = context();
        add_chain_owner(&mut context, caller);

        let data = IArbOwner::isChainOwnerCall { addr: caller }.abi_encode();
        let mut input = input(&data, caller, false, 60, &mut context);
        input.gas = BASE_PRECOMPILE_GAS + WORD_COPY_GAS + STORAGE_READ_GAS + WORD_COPY_GAS - 1;
        let output = ArbOwner::run(input).expect("post-auth OOG should become a free revert");

        assert!(output.reverted);
        assert_eq!(output.gas_used, 0);
        assert!(output.bytes.is_empty());

        let logs = context.journal_mut().take_logs();
        assert!(!logs.iter().any(|log| {
            log.address == ARB_OWNER_ADDRESS
                && log.data.topics().first() == Some(&keccak256("OwnerActs(bytes4,address,bytes)"))
        }));
    }
}
