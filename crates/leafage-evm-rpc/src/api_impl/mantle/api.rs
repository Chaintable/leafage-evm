use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::core::{ApiCore, EvmExecutor, GasProvider, TxSetter};
use crate::api_impl::mantle::evm::{create_mantle_evm_from_state, create_mantle_txn_env};
use crate::api_impl::ApiImpl;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::mantle::{MantleHardfork, GAS_ORACLE_ADDR, TOKEN_RATIO_SLOT};
use leafage_evm_types::CallRequest;
use op_revm::transaction::OpTxTr;
use op_revm::{OpHaltReason, OpTransaction, OpTransactionError};
use revm::context::result::{EVMError, ResultGas};
use revm::context::{result::ExecutionResult, BlockEnv, TxEnv};
use revm::inspector::NoOpInspector;
use revm::ExecuteEvm;
use revm::InspectCommitEvm;
use revm::{DatabaseCommit, DatabaseRef};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

type MantleApiImpl<DB> = ApiImpl<DB, MantleHardfork, NoneEvmCustomConfig>;

/// Scales all `ResultGas` fields by `token_ratio` to convert from EVM-gas to MNT-gas.
///
/// All fields (including `floor_gas` and `intrinsic_gas`) must be scaled because
/// they are in the canonical EVM-gas dimension and the output is in MNT-gas (= EVM-gas * ratio).
/// Equivalence: `scaled.used() == unscaled.used() * ratio` (same as revm 33 `gas_used * ratio`).
fn scale_result_gas(gas: ResultGas, ratio: u64) -> ResultGas {
    ResultGas::new(
        gas.limit() * ratio,
        gas.spent() * ratio,
        gas.inner_refunded() * ratio,
        gas.floor_gas() * ratio,
        gas.intrinsic_gas() * ratio,
    )
}

fn get_token_ratio<DB: DatabaseRef>(db: &DB) -> u64 {
    match db.storage_ref(GAS_ORACLE_ADDR, TOKEN_RATIO_SLOT) {
        Ok(storage_value) => storage_value.to::<u64>(),
        Err(_) => 1,
    }
}

impl<DB> GasProvider for MantleApiImpl<DB> where DB: Sync + Send + 'static {}

impl<DB> EvmExecutor for MantleApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = OpTransaction<TxEnv>;
    type TransactionError = OpTransactionError;
    type EvmHaltReason = OpHaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        create_mantle_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)
    }

    fn transact<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        mut tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let token_ratio = get_token_ratio(&state);

        let should_apply_ratio = token_ratio > 1 && !tx.is_deposit() && !tx.is_system_transaction();

        if should_apply_ratio {
            let original_gas_limit = tx.base.gas_limit;
            tx.set_gas_limit(original_gas_limit / token_ratio);
        }

        let mut evm = create_mantle_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            NoOpInspector {},
        );

        let result = evm.transact(tx)?;

        let final_result = if should_apply_ratio {
            match result.result {
                ExecutionResult::Success {
                    gas,
                    output,
                    logs,
                    reason,
                } => ExecutionResult::Success {
                    gas: scale_result_gas(gas, token_ratio),
                    output,
                    logs,
                    reason,
                },
                ExecutionResult::Revert { gas, output, logs } => ExecutionResult::Revert {
                    gas: scale_result_gas(gas, token_ratio),
                    output,
                    logs,
                },
                ExecutionResult::Halt { reason, gas, logs } => ExecutionResult::Halt {
                    reason,
                    gas: scale_result_gas(gas, token_ratio),
                    logs,
                },
            }
        } else {
            result.result
        };

        Ok(final_result.into())
    }

    fn inspect_tx_commit<
        StateDB: DatabaseRef + DatabaseCommit,
        R,
        F: FnOnce(TracingInspector) -> R,
    >(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        mut tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    > {
        let token_ratio = get_token_ratio(&state);

        let should_apply_ratio = token_ratio > 1 && !tx.is_deposit() && !tx.is_system_transaction();

        if should_apply_ratio {
            let original_gas_limit = tx.base.gas_limit;
            tx.set_gas_limit(original_gas_limit / token_ratio);
        }

        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = create_mantle_evm_from_state(
            block_env.clone(),
            self.evm_cfg.cfg.clone(),
            state,
            &mut inspector,
        );

        let result = evm.inspect_tx_commit(tx)?;

        let final_result = if should_apply_ratio {
            match result {
                ExecutionResult::Success {
                    gas,
                    output,
                    logs,
                    reason,
                } => ExecutionResult::Success {
                    gas: scale_result_gas(gas, token_ratio),
                    output,
                    logs,
                    reason,
                },
                ExecutionResult::Revert { gas, output, logs } => ExecutionResult::Revert {
                    gas: scale_result_gas(gas, token_ratio),
                    output,
                    logs,
                },
                ExecutionResult::Halt { reason, gas, logs } => ExecutionResult::Halt {
                    reason,
                    gas: scale_result_gas(gas, token_ratio),
                    logs,
                },
            }
        } else {
            result
        };

        Ok((final_result.into(), inspector_collect(inspector)))
    }
}

impl<DB> ApiCore for MantleApiImpl<DB> where DB: Sync + Send + 'static {}

#[cfg(test)]
mod tests {
    use super::scale_result_gas;
    use revm::context::result::ResultGas;

    #[test]
    fn test_ratio_scaling_equals_used_times_ratio() {
        // Verify: scaled.used() == unscaled.used() * ratio (revm 33 equivalence)
        let ratio = 3u64;

        // Case 1: normal execution (spent > floor)
        let evm_gas = ResultGas::new(50000, 40000, 2000, 30000, 21000);
        assert_eq!(evm_gas.used(), 38000);
        let scaled = scale_result_gas(evm_gas, ratio);
        assert_eq!(scaled.used(), 38000 * ratio, "normal: scaled.used() == unscaled.used() * ratio");

        // Case 2: floor kicks in (spent - refund < floor)
        let evm_gas = ResultGas::new(50000, 25000, 0, 30000, 21000);
        assert_eq!(evm_gas.used(), 30000);
        let scaled = scale_result_gas(evm_gas, ratio);
        assert_eq!(scaled.used(), 30000 * ratio, "floor: scaled.used() == floor * ratio");

        // Case 3: heavy refund
        let evm_gas = ResultGas::new(50000, 45000, 20000, 30000, 21000);
        assert_eq!(evm_gas.used(), 30000);
        let scaled = scale_result_gas(evm_gas, ratio);
        assert_eq!(scaled.used(), 30000 * ratio, "refund+floor: scaled.used() == floor * ratio");
    }

    #[test]
    fn test_ratio_1_is_identity() {
        let evm_gas = ResultGas::new(100000, 60000, 5000, 30000, 21000);
        let scaled = scale_result_gas(evm_gas, 1);
        assert_eq!(scaled.used(), evm_gas.used());
        assert_eq!(scaled.limit(), evm_gas.limit());
        assert_eq!(scaled.spent(), evm_gas.spent());
    }

    #[test]
    fn test_floor_gas_must_scale_with_ratio() {
        // Proves: NOT scaling floor_gas under-charges the user.
        // ratio=2, execution cheap, floor kicks in.
        let ratio = 2u64;
        let evm_gas = ResultGas::new(50000, 10000, 0, 30000, 21000);
        // EVM: used = max(10000, 30000) = 30000
        // MNT: should pay 30000 * 2 = 60000

        // Production function (scales floor):
        let scaled = scale_result_gas(evm_gas, ratio);
        assert_eq!(scaled.used(), 60000, "production: floor*ratio=60000");

        // Hypothetical bug (don't scale floor):
        let wrong = ResultGas::new(
            evm_gas.limit() * ratio,
            evm_gas.spent() * ratio,
            evm_gas.inner_refunded() * ratio,
            evm_gas.floor_gas(),      // BUG: not scaled
            evm_gas.intrinsic_gas(),
        );
        assert_eq!(wrong.used(), 30000, "bug: unscaled floor=30000, under-charges");
        assert_ne!(wrong.used(), scaled.used(), "bug gives different result than production");
    }

    #[test]
    fn test_floor_zero_scaling_irrelevant() {
        // When floor=0 (EIP-7623 not active), scaling doesn't matter.
        let ratio = 5u64;
        let evm_gas = ResultGas::new(100000, 50000, 3000, 0, 21000);
        let scaled = scale_result_gas(evm_gas, ratio);
        assert_eq!(scaled.used(), (50000 - 3000) * ratio);
    }
}
