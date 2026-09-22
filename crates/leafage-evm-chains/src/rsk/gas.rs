//! Gas schedule deviations from Ethereum (rskj `GasCost`, `VM`, `Program`,
//! `TransactionExecutor`).
//!
//! RSK froze its schedule around EIP-150 and cherry-picked later changes:
//!
//! * no EIP-2929 / EIP-2930: there is no warm / cold distinction. `SLOAD` is
//!   200, `BALANCE` / `EXTCODEHASH` 400, `EXTCODESIZE` / `EXTCODECOPY` / the
//!   call family 700, `SELFDESTRUCT` 5000 (static costs, see
//!   `rsk/instructions.rs`);
//! * no net gas metering (EIP-1283 / EIP-2200): `SSTORE` is the three-case
//!   Frontier rule (`VM.doSSTORE`);
//! * no EIP-150 63/64 rule: `CALL` forwards `min(remaining, requested)` and
//!   `CREATE` forwards everything that is left (`VM.getMessageCall`,
//!   `Program.createContract`);
//! * no EIP-3529: refunds are capped at half of the gas used and
//!   `SELFDESTRUCT` still refunds 24 000 (`TransactionExecutor.refundGas`).
//!
//! What RSK did adopt is already what revm does at Cancun: EIP-2028 calldata
//! cost (RSKIP400), EIP-3860 initcode metering (RSKIP438), EIP-3541 (RSKIP544).
//!
//! The values are the ones of the latest network upgrade. Activation heights
//! are not modelled: a simulation at a historical block is priced with the
//! current schedule.

use revm::context_interface::cfg::gas_params::{GasId, GasParams};
use revm::primitives::hardfork::SpecId;

/// `GasCost.SLOAD`.
pub(crate) const SLOAD: u64 = 200;
/// `GasCost.BALANCE`.
pub(crate) const BALANCE: u64 = 400;
/// `GasCost.EXT_CODE_HASH`.
pub(crate) const EXT_CODE_HASH: u64 = 400;
/// `GasCost.EXT_CODE_SIZE` / `GasCost.EXT_CODE_COPY`.
pub(crate) const EXT_CODE: u64 = 700;
/// `GasCost.CALL`, the base cost of the whole call family.
pub(crate) const CALL: u64 = 700;
/// `GasCost.SUICIDE`.
pub(crate) const SELFDESTRUCT: u64 = 5_000;

/// `GasCost.SET_SSTORE`: zero to non-zero.
pub(crate) const SSTORE_SET: u64 = 20_000;
/// `GasCost.RESET_SSTORE` / `GasCost.CLEAR_SSTORE`: every other write.
pub(crate) const SSTORE_RESET: u64 = 5_000;
/// `GasCost.REFUND_SSTORE`: non-zero to zero.
pub(crate) const SSTORE_CLEAR_REFUND: i64 = 15_000;

/// `GasCost.SUICIDE_REFUND`.
pub(crate) const SELFDESTRUCT_REFUND: u64 = 24_000;
/// `GasCost.NEW_ACCT_SUICIDE`.
pub(crate) const SELFDESTRUCT_NEW_ACCOUNT: u64 = 25_000;

/// `Program.getMaxDepth()` since RSKIP150 (Ethereum: 1024).
pub const MAX_CALL_DEPTH: usize = 400;

pub(crate) fn rsk_gas_params(spec: SpecId) -> GasParams {
    let mut gas_params = GasParams::new_spec(spec);
    gas_params.override_gas([
        // No EIP-2929. revm's Berlin code paths stay in place and add zero.
        (GasId::cold_account_additional_cost(), 0),
        (GasId::cold_storage_additional_cost(), 0),
        (GasId::cold_storage_cost(), 0),
        (GasId::warm_storage_read_cost(), 0),
        // No EIP-150 63/64 rule: `remaining - remaining / u64::MAX` keeps
        // everything. Drives DELEGATECALL, STATICCALL, CREATE and CREATE2; CALL
        // and CALLCODE are replaced because of the stipend.
        (GasId::call_stipend_reduction(), u64::MAX),
        // No EIP-3529.
        (GasId::selfdestruct_refund(), SELFDESTRUCT_REFUND),
        (
            GasId::new_account_cost_for_selfdestruct(),
            SELFDESTRUCT_NEW_ACCOUNT,
        ),
    ]);
    gas_params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_cold_costs_are_zero() {
        let gas = rsk_gas_params(SpecId::CANCUN);
        assert_eq!(gas.cold_account_additional_cost(), 0);
        assert_eq!(gas.cold_storage_additional_cost(), 0);
        assert_eq!(gas.cold_storage_cost(), 0);
        assert_eq!(gas.warm_storage_read_cost(), 0);
        assert_eq!(gas.selfdestruct_cold_cost(), 0);
    }

    #[test]
    fn all_the_remaining_gas_can_be_forwarded() {
        let gas = rsk_gas_params(SpecId::CANCUN);
        for remaining in [0, 1, 63, 64, 1_000_000, u64::MAX - 1] {
            assert_eq!(gas.call_stipend_reduction(remaining), remaining);
        }
    }

    #[test]
    fn values_shared_with_ethereum_are_untouched() {
        let gas = rsk_gas_params(SpecId::CANCUN);
        assert_eq!(gas.transfer_value_cost(), 9_000); // GasCost.VT_CALL
        assert_eq!(gas.call_stipend(), 2_300); // GasCost.STIPEND_CALL
        assert_eq!(gas.new_account_cost(false, false), 25_000); // GasCost.NEW_ACCT_CALL
        assert_eq!(gas.create_cost(), 32_000); // GasCost.CREATE
        assert_eq!(gas.code_deposit_cost(1), 200); // GasCost.CREATE_DATA
        assert_eq!(gas.initcode_cost(32), 2); // GasCost.INITCODE_WORD_COST
        assert_eq!(gas.selfdestruct_refund(), 24_000);
    }
}
