use crate::polygon::PolygonHardfork;
use leafage_evm_types::CfgEnv;
use revm::context_interface::cfg::gas_params::{GasId, GasParams};
use revm::primitives::hardfork::SpecId;

pub(crate) const TX_GAS_LIMIT_CAP_POST_MADHUGIRI: u64 = 1 << 25;

pub(crate) const WARM_STORAGE_READ_COST: u64 = 100;
const COLD_SLOAD_COST_PIP88: u64 = 5_460;
pub(crate) const COLD_SLOAD_ADDITIONAL_COST_PIP88: u64 =
    COLD_SLOAD_COST_PIP88 - WARM_STORAGE_READ_COST;
pub(crate) const COLD_SSTORE_COST_PIP88: u64 = 2_940;
pub(crate) const COLD_SSTORE_ADDITIONAL_COST_PIP88: u64 =
    COLD_SSTORE_COST_PIP88 - WARM_STORAGE_READ_COST;
const SSTORE_RESET_GAS_EIP2200: u64 = 5_000;
const TX_ACCESS_LIST_STORAGE_KEY_GAS: u64 = 1_900;
pub(crate) const SSTORE_RESET_WITHOUT_COLD_LOAD_COST_PIP88: u64 =
    SSTORE_RESET_GAS_EIP2200 - COLD_SSTORE_COST_PIP88 - WARM_STORAGE_READ_COST;
pub(crate) const SSTORE_CLEARS_SCHEDULE_REFUND_PIP88: u64 =
    SSTORE_RESET_GAS_EIP2200 - COLD_SSTORE_COST_PIP88 + TX_ACCESS_LIST_STORAGE_KEY_GAS;

pub(crate) fn apply_gas_rules(hardfork: PolygonHardfork, cfg: &mut CfgEnv<PolygonHardfork>) {
    apply_storage_gas(hardfork, cfg);
    apply_tx_gas_limit_cap(hardfork, cfg);
}

fn apply_storage_gas(hardfork: PolygonHardfork, cfg: &mut CfgEnv<PolygonHardfork>) {
    if hardfork.is_pip88_enabled() {
        cfg.set_gas_params(pip88_gas_params(hardfork.into()));
    }
}

fn apply_tx_gas_limit_cap(hardfork: PolygonHardfork, cfg: &mut CfgEnv<PolygonHardfork>) {
    let Some(limit) = tx_gas_limit_cap(hardfork) else {
        return;
    };

    cfg.tx_gas_limit_cap = Some(capped_tx_gas_limit(cfg.tx_gas_limit_cap, limit));
}

fn tx_gas_limit_cap(hardfork: PolygonHardfork) -> Option<u64> {
    hardfork
        .is_madhugiri_enabled()
        .then_some(TX_GAS_LIMIT_CAP_POST_MADHUGIRI)
}

fn pip88_gas_params(spec: SpecId) -> GasParams {
    let mut gas_params = GasParams::new_spec(spec);
    apply_pip88_storage_gas(&mut gas_params);
    gas_params
}

fn apply_pip88_storage_gas(gas_params: &mut GasParams) {
    gas_params.override_gas(pip88_storage_overrides());
}

fn pip88_storage_overrides() -> impl Iterator<Item = (GasId, u64)> {
    [
        (
            GasId::cold_storage_additional_cost(),
            COLD_SSTORE_ADDITIONAL_COST_PIP88,
        ),
        (GasId::cold_storage_cost(), COLD_SSTORE_COST_PIP88),
        (
            GasId::sstore_reset_without_cold_load_cost(),
            SSTORE_RESET_WITHOUT_COLD_LOAD_COST_PIP88,
        ),
        (
            GasId::sstore_clearing_slot_refund(),
            SSTORE_CLEARS_SCHEDULE_REFUND_PIP88,
        ),
        (
            GasId::sstore_reset_refund(),
            SSTORE_RESET_WITHOUT_COLD_LOAD_COST_PIP88,
        ),
    ]
    .into_iter()
}

fn capped_tx_gas_limit(current: Option<u64>, limit: u64) -> u64 {
    current.filter(|cap| *cap != 0).unwrap_or(limit).min(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::context_interface::context::SStoreResult;
    use revm::primitives::U256;

    fn slot(original_value: U256, present_value: U256, new_value: U256) -> SStoreResult {
        SStoreResult {
            original_value,
            present_value,
            new_value,
        }
    }

    #[test]
    fn pip88_sstore_branches_match_bor_formulas() {
        let gas = pip88_gas_params(SpecId::PRAGUE);
        let val42 = U256::from(0x42);
        let val99 = U256::from(0x99);

        let total = |state: &SStoreResult, is_cold| {
            gas.sstore_static_gas() + gas.sstore_dynamic_gas(true, state, is_cold)
        };

        let cold_reset_existing = slot(val42, val42, val99);
        assert_eq!(total(&cold_reset_existing, true), SSTORE_RESET_GAS_EIP2200);

        let warm_reset_existing = slot(val42, val42, val99);
        assert_eq!(
            total(&warm_reset_existing, false),
            SSTORE_RESET_GAS_EIP2200 - COLD_SSTORE_COST_PIP88
        );

        let cold_create = slot(U256::ZERO, U256::ZERO, val99);
        assert_eq!(total(&cold_create, true), COLD_SSTORE_COST_PIP88 + 20_000);

        let cold_noop = slot(val42, val42, val42);
        assert_eq!(
            total(&cold_noop, true),
            COLD_SSTORE_COST_PIP88 + WARM_STORAGE_READ_COST
        );

        let reset_to_original_existing = slot(val42, val99, val42);
        assert_eq!(
            gas.sstore_refund(true, &reset_to_original_existing),
            SSTORE_RESET_WITHOUT_COLD_LOAD_COST_PIP88 as i64
        );
    }

    #[test]
    fn tx_gas_cap_preserves_stricter_existing_limit() {
        assert_eq!(
            capped_tx_gas_limit(Some(21_000), TX_GAS_LIMIT_CAP_POST_MADHUGIRI),
            21_000
        );
        assert_eq!(
            capped_tx_gas_limit(Some(u64::MAX), TX_GAS_LIMIT_CAP_POST_MADHUGIRI),
            TX_GAS_LIMIT_CAP_POST_MADHUGIRI
        );
        assert_eq!(
            capped_tx_gas_limit(None, TX_GAS_LIMIT_CAP_POST_MADHUGIRI),
            TX_GAS_LIMIT_CAP_POST_MADHUGIRI
        );
        assert_eq!(
            capped_tx_gas_limit(Some(0), TX_GAS_LIMIT_CAP_POST_MADHUGIRI),
            TX_GAS_LIMIT_CAP_POST_MADHUGIRI
        );
    }
}
