//! Ethermint v0.20.1-cronos introduced the fee-market minimum on Cronos at
//! Galileo. Its v011 store migration sets MinGasMultiplier to 0.5. Cosmos SDK
//! stores the decimal with 18 fractional digits and truncates the product.
use anyhow::{ensure, Result};

const DECIMAL_SCALE: u64 = 1_000_000_000_000_000_000;

#[derive(Clone, Copy, Debug)]
pub(super) enum GasBasis {
    /// v1.0.2 and v1.0.15 capture temporaryGasUsed after adding GasToRefund.
    AfterRefund,
    /// v1.1.0 through v1.7.0 capture temporaryGasUsed before adding the refund.
    BeforeRefund,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SettledGas {
    pub(super) cosmos: u64,
    pub(super) receipt: u64,
    pub(super) charged: u64,
}

impl GasBasis {
    /// The two execution gas inputs are measured by the VM before applying
    /// Ethermint's multiplier. A receipt or an ABCI result is never an input.
    pub(super) fn settle(
        self,
        limit: u64,
        spent_before_refund: u64,
        spent_after_refund: u64,
        multiplier_atomics: u64,
    ) -> Result<SettledGas> {
        ensure!(
            spent_after_refund <= spent_before_refund && spent_before_refund <= limit,
            "invalid Cronos execution gas gross={spent_before_refund} net={spent_after_refund} limit={limit}"
        );
        ensure!(
            multiplier_atomics <= DECIMAL_SCALE,
            "invalid Cronos MinGasMultiplier {multiplier_atomics} atomics"
        );
        let minimum =
            u128::from(limit) * u128::from(multiplier_atomics) / u128::from(DECIMAL_SCALE);
        let measured = match self {
            Self::AfterRefund => spent_after_refund,
            Self::BeforeRefund => spent_before_refund,
        };
        let charged = measured.max(minimum as u64);
        // ApplyTransaction refunds (limit - res.GasUsed), places res.GasUsed in
        // the receipt, and resets the Cosmos message meter to res.GasUsed.
        Ok(SettledGas {
            cosmos: charged,
            receipt: charged,
            charged,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GALILEO_MIN_GAS_MULTIPLIER: u64 = DECIMAL_SCALE / 2;

    #[test]
    fn titan_measures_gas_before_the_storage_refund() {
        let settle = |basis: GasBasis| {
            basis
                .settle(100_000, 68_000, 54_400, GALILEO_MIN_GAS_MULTIPLIER)
                .unwrap()
        };
        assert_eq!(settle(GasBasis::AfterRefund).charged, 54_400);
        assert_eq!(settle(GasBasis::BeforeRefund).charged, 68_000);
    }

    #[test]
    fn cosmos_minimum_truncates_and_controls_wallet_and_receipt() {
        let result = GasBasis::AfterRefund
            .settle(65_833, 21_944, 21_944, GALILEO_MIN_GAS_MULTIPLIER)
            .unwrap();
        assert_eq!(
            result,
            SettledGas {
                cosmos: 32_916,
                receipt: 32_916,
                charged: 32_916
            }
        );
    }
}
