//! Gas schedule deviations from Ethereum.
//!
//! * `monad_transaction_gas.cpp`: refunds are zero from MONAD_ONE.
//! * `traits.hpp` `MonadTraits::cold_account_cost / cold_storage_cost`: with
//!   pricing v1 (MONAD_SEVEN) the *additional* cold cost is 10 000 for
//!   accounts and 8 000 for storage slots (Ethereum: 2 500 / 2 000).
//! * `vm/runtime/types.hpp`: MIP-3 memory model.
//! * `traits.hpp` `base_sstore_cost / page_write_cost / page_growth_cost`:
//!   MIP-8 storage model.

use crate::monad::MonadHardfork;
use leafage_evm_types::CfgEnv;
use revm::context_interface::cfg::gas_params::{GasId, GasParams};

/// Warm SLOAD / SSTORE base cost, unchanged from Ethereum.
pub(crate) const WARM_STORAGE_READ_COST: u64 = 100;
/// `MonadTraits::cold_account_cost()` with pricing v1 (charged on top of the
/// warm cost).
pub(crate) const COLD_ACCOUNT_ADDITIONAL_COST_V1: u64 = 10_000;
/// `MonadTraits::cold_storage_cost()` with pricing v1 (charged on top of the
/// warm cost).
pub(crate) const COLD_STORAGE_ADDITIONAL_COST_V1: u64 = 8_000;
pub(crate) const COLD_STORAGE_COST_V1: u64 =
    WARM_STORAGE_READ_COST + COLD_STORAGE_ADDITIONAL_COST_V1;

/// MIP-3: transaction wide memory cap (`is_memory_size_in_bound`).
pub(crate) const MIP3_MEMORY_LIMIT: usize = 0x80_0000; // 8 MB

/// MIP-8: `MonadTraits::base_sstore_cost()`.
pub(crate) const MIP8_BASE_SSTORE_COST: u64 = 100;
/// MIP-8: `MonadTraits::page_write_cost()`, first value-changing write to a
/// page inside the transaction.
pub(crate) const MIP8_PAGE_WRITE_COST: u64 = 2_800;
/// MIP-8: `MonadTraits::page_growth_cost()`, page grows past its peak size
/// inside the transaction.
pub(crate) const MIP8_PAGE_GROWTH_COST: u64 = 17_000;

/// MIP-3 memory expansion cost for `words` words (`memory_cost_from_word_count`).
#[inline]
pub(crate) const fn mip3_memory_cost(words: usize) -> u64 {
    (words as u64) >> 1
}

pub(crate) fn apply_gas_rules(hardfork: MonadHardfork, cfg: &mut CfgEnv<MonadHardfork>) {
    cfg.set_gas_params(monad_gas_params(hardfork));
}

pub(crate) fn monad_gas_params(hardfork: MonadHardfork) -> GasParams {
    let mut gas_params = GasParams::new_spec(hardfork.into());
    // MONAD_ONE: refunds are never paid out. The handler also zeroes the final
    // refund; this keeps every intermediate counter consistent with that.
    gas_params.override_gas([
        (GasId::sstore_clearing_slot_refund(), 0),
        (GasId::sstore_set_refund(), 0),
        (GasId::sstore_reset_refund(), 0),
        (GasId::selfdestruct_refund(), 0),
        (GasId::tx_eip7702_auth_refund(), 0),
    ]);
    if hardfork.is_pricing_v1_enabled() {
        gas_params.override_gas([
            (
                GasId::cold_account_additional_cost(),
                COLD_ACCOUNT_ADDITIONAL_COST_V1,
            ),
            (
                GasId::cold_storage_additional_cost(),
                COLD_STORAGE_ADDITIONAL_COST_V1,
            ),
            (GasId::cold_storage_cost(), COLD_STORAGE_COST_V1),
        ]);
    }
    gas_params
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
    fn pricing_v1_cold_costs() {
        let gas = monad_gas_params(MonadHardfork::MonadSeven);
        assert_eq!(gas.cold_account_additional_cost(), 10_000);
        assert_eq!(gas.cold_storage_additional_cost(), 8_000);
        assert_eq!(gas.cold_storage_cost(), 8_100);
        // BALANCE / EXTCODE* / CALL cold: 100 + 10 000.
        assert_eq!(gas.selfdestruct_cold_cost(), 10_100);

        let gas = monad_gas_params(MonadHardfork::MonadSix);
        assert_eq!(gas.cold_account_additional_cost(), 2_500);
        assert_eq!(gas.cold_storage_additional_cost(), 2_000);
    }

    #[test]
    fn sstore_costs_match_monad_storage_cost_table() {
        // storage_costs.hpp: cold access adds cold_storage_cost (8000) + the
        // 100 static gas that is refunded via `gas_used -= min_gas`.
        let gas = monad_gas_params(MonadHardfork::MonadSeven);
        let x = U256::from(0x42);
        let z = U256::from(0x99);
        let total = |state: &SStoreResult, is_cold| {
            gas.sstore_static_gas() + gas.sstore_dynamic_gas(true, state, is_cold)
        };
        // ADDED, cold: 100 + 8000 + 20000
        assert_eq!(total(&slot(U256::ZERO, U256::ZERO, z), true), 28_100);
        // ADDED, warm: 20000
        assert_eq!(total(&slot(U256::ZERO, U256::ZERO, z), false), 20_000);
        // MODIFIED, cold: 100 + 8000 + 2900
        assert_eq!(total(&slot(x, x, z), true), 11_000);
        // ASSIGNED, warm: 100
        assert_eq!(total(&slot(x, z, z), false), 100);
        // refunds are always zero
        assert_eq!(gas.sstore_refund(true, &slot(x, x, U256::ZERO)), 0);
        assert_eq!(gas.sstore_refund(true, &slot(x, z, x)), 0);
        assert_eq!(gas.selfdestruct_refund(), 0);
    }

    #[test]
    fn mip3_memory_cost_is_half_word_count() {
        assert_eq!(mip3_memory_cost(0), 0);
        assert_eq!(mip3_memory_cost(1), 0);
        assert_eq!(mip3_memory_cost(2), 1);
        assert_eq!(mip3_memory_cost(1025), 512);
    }
}
