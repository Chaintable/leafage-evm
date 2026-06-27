use super::abi::{IArbGasInfo, MultiGasConstraint, WeightedResource};
use super::state::ArbStorage;
use super::util::{dispatch, empty_revert, finish_call, low_u64_as_i64, signed_word};
use super::{ArbPrecompileInput, ArbitrumContext};
use crate::arbitrum::arbos_state;
use revm::precompile::PrecompileResult;
use revm::Database;

const ARBOS_VERSION_10: u64 = 10;
const ARBOS_VERSION_11: u64 = 11;
const ARBOS_VERSION_20: u64 = 20;
const ARBOS_VERSION_50: u64 = 50;
const ARBOS_VERSION_60: u64 = 60;

pub(super) struct ArbGasInfo;

impl ArbGasInfo {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let current_arbos_version = input.current_arbos_version;
        let current_tx_l1_gas_fees = input.current_tx_l1_gas_fees;
        let context = input.context;
        dispatch::<IArbGasInfo::IArbGasInfoCalls>(data, gas_limit, |call, initial_gas| {
            if current_arbos_version < Self::required_arbos_version(&call) {
                return empty_revert(gas_limit, gas_limit);
            }

            let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, initial_gas);
            match call {
                IArbGasInfo::IArbGasInfoCalls::getPricesInWeiWithAggregator(_)
                | IArbGasInfo::IArbGasInfoCalls::getPricesInWei(_) => {
                    let ret = storage.gas_prices_in_wei(current_arbos_version)?;
                    match call {
                        IArbGasInfo::IArbGasInfoCalls::getPricesInWeiWithAggregator(_) => {
                            finish_call::<IArbGasInfo::getPricesInWeiWithAggregatorCall>(
                                gas_limit,
                                storage.gas_used,
                                ret.into(),
                            )
                        }
                        _ => finish_call::<IArbGasInfo::getPricesInWeiCall>(
                            gas_limit,
                            storage.gas_used,
                            ret.into(),
                        ),
                    }
                }
                IArbGasInfo::IArbGasInfoCalls::getPricesInArbGasWithAggregator(_)
                | IArbGasInfo::IArbGasInfoCalls::getPricesInArbGas(_) => {
                    let ret = storage.gas_prices_in_arb_gas(current_arbos_version)?;
                    match call {
                        IArbGasInfo::IArbGasInfoCalls::getPricesInArbGasWithAggregator(_) => {
                            finish_call::<IArbGasInfo::getPricesInArbGasWithAggregatorCall>(
                                gas_limit,
                                storage.gas_used,
                                ret.into(),
                            )
                        }
                        _ => finish_call::<IArbGasInfo::getPricesInArbGasCall>(
                            gas_limit,
                            storage.gas_used,
                            ret.into(),
                        ),
                    }
                }
                IArbGasInfo::IArbGasInfoCalls::getGasAccountingParams(_) => {
                    let l2_key = storage.l2_key();
                    let speed =
                        storage.read(&l2_key, arbos_state::L2_SPEED_LIMIT_PER_SECOND_OFFSET)?;
                    let max = storage.read(&l2_key, arbos_state::L2_PER_BLOCK_GAS_LIMIT_OFFSET)?;
                    finish_call::<IArbGasInfo::getGasAccountingParamsCall>(
                        gas_limit,
                        storage.gas_used,
                        (speed, max, max).into(),
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getMaxTxGasLimit(_) => {
                    let l2_key = storage.l2_key();
                    let value = storage.read(&l2_key, arbos_state::L2_PER_TX_GAS_LIMIT_OFFSET)?;
                    finish_call::<IArbGasInfo::getMaxTxGasLimitCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getMinimumGasPrice(_) => {
                    let l2_key = storage.l2_key();
                    let value = storage.read(&l2_key, arbos_state::L2_MIN_BASE_FEE_WEI_OFFSET)?;
                    finish_call::<IArbGasInfo::getMinimumGasPriceCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getL1BaseFeeEstimate(_)
                | IArbGasInfo::IArbGasInfoCalls::getL1GasPriceEstimate(_) => {
                    let l1_key = storage.l1_key();
                    let value = storage.read(&l1_key, arbos_state::L1_PRICE_PER_UNIT_OFFSET)?;
                    match call {
                        IArbGasInfo::IArbGasInfoCalls::getL1BaseFeeEstimate(_) => {
                            finish_call::<IArbGasInfo::getL1BaseFeeEstimateCall>(
                                gas_limit,
                                storage.gas_used,
                                value,
                            )
                        }
                        _ => finish_call::<IArbGasInfo::getL1GasPriceEstimateCall>(
                            gas_limit,
                            storage.gas_used,
                            value,
                        ),
                    }
                }
                IArbGasInfo::IArbGasInfoCalls::getL1BaseFeeEstimateInertia(_) => {
                    let l1_key = storage.l1_key();
                    let value = storage.read_u64(&l1_key, arbos_state::L1_INERTIA_OFFSET)?;
                    finish_call::<IArbGasInfo::getL1BaseFeeEstimateInertiaCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getL1RewardRate(_) => {
                    let l1_key = storage.l1_key();
                    let value =
                        storage.read_u64(&l1_key, arbos_state::L1_PER_UNIT_REWARD_OFFSET)?;
                    finish_call::<IArbGasInfo::getL1RewardRateCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getL1RewardRecipient(_) => {
                    let l1_key = storage.l1_key();
                    let value =
                        storage.read_address(&l1_key, arbos_state::L1_PAY_REWARDS_TO_OFFSET)?;
                    finish_call::<IArbGasInfo::getL1RewardRecipientCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getCurrentTxL1GasFees(_) => {
                    finish_call::<IArbGasInfo::getCurrentTxL1GasFeesCall>(
                        gas_limit,
                        storage.gas_used,
                        current_tx_l1_gas_fees,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getGasBacklog(_) => {
                    let l2_key = storage.l2_key();
                    let value = storage.read_u64(&l2_key, arbos_state::L2_GAS_BACKLOG_OFFSET)?;
                    finish_call::<IArbGasInfo::getGasBacklogCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getPricingInertia(_) => {
                    let l2_key = storage.l2_key();
                    let value =
                        storage.read_u64(&l2_key, arbos_state::L2_PRICING_INERTIA_OFFSET)?;
                    finish_call::<IArbGasInfo::getPricingInertiaCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getGasBacklogTolerance(_) => {
                    let l2_key = storage.l2_key();
                    let value =
                        storage.read_u64(&l2_key, arbos_state::L2_BACKLOG_TOLERANCE_OFFSET)?;
                    finish_call::<IArbGasInfo::getGasBacklogToleranceCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getL1PricingSurplus(_) => {
                    let value = storage.l1_pricing_surplus()?;
                    finish_call::<IArbGasInfo::getL1PricingSurplusCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getPerBatchGasCharge(_) => {
                    let l1_key = storage.l1_key();
                    let value = low_u64_as_i64(
                        storage.read(&l1_key, arbos_state::L1_PER_BATCH_GAS_COST_OFFSET)?,
                    );
                    finish_call::<IArbGasInfo::getPerBatchGasChargeCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getAmortizedCostCapBips(_) => {
                    let l1_key = storage.l1_key();
                    let value = storage
                        .read_u64(&l1_key, arbos_state::L1_AMORTIZED_COST_CAP_BIPS_OFFSET)?;
                    finish_call::<IArbGasInfo::getAmortizedCostCapBipsCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getL1FeesAvailable(_) => {
                    let l1_key = storage.l1_key();
                    let value = storage.read(&l1_key, arbos_state::L1_FEES_AVAILABLE_OFFSET)?;
                    finish_call::<IArbGasInfo::getL1FeesAvailableCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getL1PricingEquilibrationUnits(_) => {
                    let l1_key = storage.l1_key();
                    let value =
                        storage.read(&l1_key, arbos_state::L1_EQUILIBRATION_UNITS_OFFSET)?;
                    finish_call::<IArbGasInfo::getL1PricingEquilibrationUnitsCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getLastL1PricingUpdateTime(_) => {
                    let l1_key = storage.l1_key();
                    let value =
                        storage.read_u64(&l1_key, arbos_state::L1_LAST_UPDATE_TIME_OFFSET)?;
                    finish_call::<IArbGasInfo::getLastL1PricingUpdateTimeCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getL1PricingFundsDueForRewards(_) => {
                    let l1_key = storage.l1_key();
                    let value =
                        storage.read(&l1_key, arbos_state::L1_FUNDS_DUE_FOR_REWARDS_OFFSET)?;
                    finish_call::<IArbGasInfo::getL1PricingFundsDueForRewardsCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getL1PricingUnitsSinceUpdate(_) => {
                    let l1_key = storage.l1_key();
                    let value =
                        storage.read_u64(&l1_key, arbos_state::L1_UNITS_SINCE_UPDATE_OFFSET)?;
                    finish_call::<IArbGasInfo::getL1PricingUnitsSinceUpdateCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getLastL1PricingSurplus(_) => {
                    let l1_key = storage.l1_key();
                    let value =
                        signed_word(storage.read(&l1_key, arbos_state::L1_LAST_SURPLUS_OFFSET)?);
                    finish_call::<IArbGasInfo::getLastL1PricingSurplusCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getMaxBlockGasLimit(_) => {
                    let l2_key = storage.l2_key();
                    let value =
                        storage.read_u64(&l2_key, arbos_state::L2_PER_BLOCK_GAS_LIMIT_OFFSET)?;
                    finish_call::<IArbGasInfo::getMaxBlockGasLimitCall>(
                        gas_limit,
                        storage.gas_used,
                        value,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getGasPricingConstraints(_) => {
                    let constraints = storage.gas_pricing_constraints()?;
                    finish_call::<IArbGasInfo::getGasPricingConstraintsCall>(
                        gas_limit,
                        storage.gas_used,
                        constraints,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getMultiGasPricingConstraints(_) => {
                    let constraints = storage
                        .multi_gas_pricing_constraints()?
                        .into_iter()
                        .map(|constraint| {
                            let resources = constraint
                                .resources
                                .iter()
                                .copied()
                                .enumerate()
                                .filter(|(_, weight)| *weight != 0)
                                .map(|(resource, weight)| WeightedResource {
                                    resource: resource as u8,
                                    weight,
                                })
                                .collect();
                            MultiGasConstraint {
                                resources,
                                adjustmentWindowSecs: constraint.adjustment_window_secs,
                                targetPerSec: constraint.target_per_sec,
                                backlog: constraint.backlog,
                            }
                        })
                        .collect::<Vec<_>>();
                    finish_call::<IArbGasInfo::getMultiGasPricingConstraintsCall>(
                        gas_limit,
                        storage.gas_used,
                        constraints,
                    )
                }
                IArbGasInfo::IArbGasInfoCalls::getMultiGasBaseFee(_) => {
                    let fees = storage.multi_gas_current_base_fees()?;
                    finish_call::<IArbGasInfo::getMultiGasBaseFeeCall>(
                        gas_limit,
                        storage.gas_used,
                        fees,
                    )
                }
            }
        })
    }

    fn required_arbos_version(call: &IArbGasInfo::IArbGasInfoCalls) -> u64 {
        match call {
            IArbGasInfo::IArbGasInfoCalls::getL1FeesAvailable(_) => ARBOS_VERSION_10,
            IArbGasInfo::IArbGasInfoCalls::getL1RewardRate(_)
            | IArbGasInfo::IArbGasInfoCalls::getL1RewardRecipient(_) => ARBOS_VERSION_11,
            IArbGasInfo::IArbGasInfoCalls::getL1PricingEquilibrationUnits(_)
            | IArbGasInfo::IArbGasInfoCalls::getLastL1PricingUpdateTime(_)
            | IArbGasInfo::IArbGasInfoCalls::getL1PricingFundsDueForRewards(_)
            | IArbGasInfo::IArbGasInfoCalls::getL1PricingUnitsSinceUpdate(_)
            | IArbGasInfo::IArbGasInfoCalls::getLastL1PricingSurplus(_) => ARBOS_VERSION_20,
            IArbGasInfo::IArbGasInfoCalls::getMaxTxGasLimit(_)
            | IArbGasInfo::IArbGasInfoCalls::getMaxBlockGasLimit(_)
            | IArbGasInfo::IArbGasInfoCalls::getGasPricingConstraints(_) => ARBOS_VERSION_50,
            IArbGasInfo::IArbGasInfoCalls::getMultiGasPricingConstraints(_)
            | IArbGasInfo::IArbGasInfoCalls::getMultiGasBaseFee(_) => ARBOS_VERSION_60,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_versions_match_nitro_registration() {
        let cases = [
            (
                IArbGasInfo::IArbGasInfoCalls::getL1FeesAvailable(
                    IArbGasInfo::getL1FeesAvailableCall {},
                ),
                ARBOS_VERSION_10,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getL1RewardRate(IArbGasInfo::getL1RewardRateCall {}),
                ARBOS_VERSION_11,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getL1RewardRecipient(
                    IArbGasInfo::getL1RewardRecipientCall {},
                ),
                ARBOS_VERSION_11,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getL1PricingEquilibrationUnits(
                    IArbGasInfo::getL1PricingEquilibrationUnitsCall {},
                ),
                ARBOS_VERSION_20,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getLastL1PricingUpdateTime(
                    IArbGasInfo::getLastL1PricingUpdateTimeCall {},
                ),
                ARBOS_VERSION_20,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getL1PricingFundsDueForRewards(
                    IArbGasInfo::getL1PricingFundsDueForRewardsCall {},
                ),
                ARBOS_VERSION_20,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getL1PricingUnitsSinceUpdate(
                    IArbGasInfo::getL1PricingUnitsSinceUpdateCall {},
                ),
                ARBOS_VERSION_20,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getLastL1PricingSurplus(
                    IArbGasInfo::getLastL1PricingSurplusCall {},
                ),
                ARBOS_VERSION_20,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getMaxTxGasLimit(
                    IArbGasInfo::getMaxTxGasLimitCall {},
                ),
                ARBOS_VERSION_50,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getMaxBlockGasLimit(
                    IArbGasInfo::getMaxBlockGasLimitCall {},
                ),
                ARBOS_VERSION_50,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getGasPricingConstraints(
                    IArbGasInfo::getGasPricingConstraintsCall {},
                ),
                ARBOS_VERSION_50,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getMultiGasPricingConstraints(
                    IArbGasInfo::getMultiGasPricingConstraintsCall {},
                ),
                ARBOS_VERSION_60,
            ),
            (
                IArbGasInfo::IArbGasInfoCalls::getMultiGasBaseFee(
                    IArbGasInfo::getMultiGasBaseFeeCall {},
                ),
                ARBOS_VERSION_60,
            ),
        ];

        for (call, version) in cases {
            assert_eq!(ArbGasInfo::required_arbos_version(&call), version);
        }

        assert_eq!(
            ArbGasInfo::required_arbos_version(&IArbGasInfo::IArbGasInfoCalls::getMinimumGasPrice(
                IArbGasInfo::getMinimumGasPriceCall {}
            )),
            0
        );
    }
}
