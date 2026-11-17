use primitive_types::U256;
use revm::context::result::ExecutionResult;
use revm::DatabaseRef;
use tokio_util::sync::CancellationToken;

use super::constants::MAX_L2_TX_GAS_LIMIT;
use super::harness::{execute_bootloader, BootloaderExecutionOutput};
use super::layout::BootloaderEnv;
use super::pubdata::derive_overhead;
use super::types::TransactionData;

pub struct GasEstimator<'a, DB> {
    db: &'a DB,
    tx: TransactionData,
    env: BootloaderEnv,
    gas_per_pubdata_byte: u64,
    acceptable_overestimation: u64,
    settlement_layer: u64,
    cancel_token: CancellationToken,
}

impl<'a, DB: DatabaseRef> GasEstimator<'a, DB> {
    pub fn new(
        db: &'a DB,
        tx: TransactionData,
        env: BootloaderEnv,
        gas_per_pubdata_byte: u64,
        acceptable_overestimation: u64,
        settlement_layer: u64,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            db,
            tx,
            env,
            gas_per_pubdata_byte,
            acceptable_overestimation,
            settlement_layer,
            cancel_token,
        }
    }

    pub fn tx_overhead(&self) -> u64 {
        derive_overhead(self.tx.encoding_len()) as u64
    }

    fn unadjusted_step(&self, forced_gas_limit: u64) -> Result<BootloaderExecutionOutput, String> {
        let mut tx = self.tx.clone();
        tx.gas_limit = U256::from(forced_gas_limit);

        execute_bootloader(
            self.db,
            &tx,
            &self.env,
            forced_gas_limit,
            self.settlement_layer,
        )
        .map_err(|e| format!("Bootloader execution error: {:?}", e))
    }

    pub fn step(&self, tx_gas_limit: u64) -> Result<BootloaderExecutionOutput, String> {
        let gas_with_overhead = (tx_gas_limit + self.tx_overhead()).min(MAX_L2_TX_GAS_LIMIT);
        self.unadjusted_step(gas_with_overhead)
    }

    pub fn estimate(&self) -> Result<u64, String> {
        // Step 1: Initial estimate with max gas limit
        let initial_output = self.unadjusted_step(MAX_L2_TX_GAS_LIMIT)?;

        match &initial_output.result {
            ExecutionResult::Success { .. } => {}
            ExecutionResult::Revert { output, .. } => {
                return Err(format!(
                    "Execution reverted: 0x{}",
                    alloy::primitives::hex::encode(output.as_ref())
                ));
            }
            ExecutionResult::Halt { reason, .. } => {
                return Err(format!("Execution halted: {:?}", reason));
            }
        }

        let operator_overhead = self.tx_overhead();
        let gas_charged_for_pubdata = initial_output.pubdata_published * self.gas_per_pubdata_byte;
        let total_gas_charged = (initial_output.gas_refunded > 0
            && initial_output.gas_refunded <= MAX_L2_TX_GAS_LIMIT)
            .then(|| MAX_L2_TX_GAS_LIMIT - initial_output.gas_refunded);

        let mut total_gas_bound = initial_output.computational_gas_used + gas_charged_for_pubdata;
        if let Some(gas_charged) = total_gas_charged {
            let safety_margin = (gas_charged / 64).max(1_000);
            total_gas_bound = total_gas_bound.min(gas_charged.saturating_sub(safety_margin));
        }

        let lower_bound = total_gas_bound
            .saturating_sub(operator_overhead)
            .max(gas_charged_for_pubdata);
        let upper_bound = MAX_L2_TX_GAS_LIMIT;

        let mut lower = lower_bound;
        let mut upper = upper_bound;

        // Step 2: Optimistic search pivot
        let optimistic_gas_limit = total_gas_charged
            .and_then(|gc| gc.checked_sub(operator_overhead))
            .map(|gc_no_ovh| gc_no_ovh.saturating_mul(21) / 20)
            .unwrap_or_else(|| lower_bound.saturating_mul(21) / 20);
        if optimistic_gas_limit > lower && optimistic_gas_limit < (lower + upper) / 2 {
            if self.cancel_token.is_cancelled() {
                return Err("Gas estimation cancelled".to_string());
            }

            let out = self.step(optimistic_gas_limit)?;
            if out.result.is_success() {
                upper = optimistic_gas_limit;
            } else {
                lower = optimistic_gas_limit + 1;
            }
        }

        // Step 3: Binary search loop
        while lower + self.acceptable_overestimation < upper {
            if self.cancel_token.is_cancelled() {
                return Err("Gas estimation cancelled".to_string());
            }

            let mid = (lower + upper) / 2;
            let out = self.step(mid)?;
            if out.result.is_success() {
                upper = mid;
            } else {
                lower = mid + 1;
            }
        }

        const ESTIMATE_GAS_SCALE_FACTOR: f64 = 1.3;
        let unscaled_gas_limit = upper;
        let suggested_gas_limit = (unscaled_gas_limit as f64 * ESTIMATE_GAS_SCALE_FACTOR) as u64;
        let final_gas_limit = suggested_gas_limit.saturating_add(operator_overhead);
        Ok(final_gas_limit)
    }
}
