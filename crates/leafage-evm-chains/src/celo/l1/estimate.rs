//! Celo call estimation: native request conversion, isolated fee settlement
//! and configured gas inflation. No RPC transport or shared-chain policy.
use super::{
    call::execute,
    consensus::CeloChainData,
    tx::{create_txn_env, CallInputError},
};
use leafage_evm_types::{BlockEnv, CallRequest, CfgEnv, MainnetSpecId};
use revm::{
    context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
    inspector::NoOpInspector,
    primitives::{Bytes, TxKind},
    DatabaseRef,
};

#[derive(Debug)]
pub enum EstimateError<E> {
    Input(CallInputError<E>),
    Execution(EVMError<E>),
    Revert(Bytes),
    Halt(HaltReason),
    Cancelled,
}

fn buffered(gas: u64, percent: u64) -> u64 {
    (u128::from(gas) * u128::from(percent.max(100)) / 100).min(u128::from(u64::MAX)) as u64
}

pub fn estimate<DB: DatabaseRef>(
    chain: &dyn CeloChainData,
    block: &BlockEnv,
    cfg: CfgEnv<MainnetSpecId>,
    state: &DB,
    mut request: CallRequest,
    buffer: u64,
    cancelled: impl Fn() -> bool,
) -> Result<u64, EstimateError<DB::Error>> {
    // Native DoEstimateGas clears prices before converting/validating a message.
    request.gas_price = None;
    request.max_fee_per_gas = None;
    request.max_priority_fee_per_gas = None;
    request.nonce = None;
    if request.gas.is_some_and(|gas| gas < 21_000) {
        request.gas = None;
    }
    let cap = cfg
        .tx_gas_limit_cap
        .filter(|&cap| cap != 0)
        .unwrap_or(u64::MAX)
        .min(block.gas_limit);
    let mut high = request.gas.map_or(cap, |gas| gas.min(cap));
    let mut tx = create_txn_env(block, cfg.clone(), request, state, cfg.chain_id)
        .map_err(EstimateError::Input)?;
    tx.set_gas_estimation();
    let run = |gas| {
        let mut probe = tx.clone();
        probe.set_gas_limit(gas);
        execute(
            chain,
            block,
            cfg.clone(),
            state,
            probe,
            NoOpInspector {},
            false,
        )
        .map(|result| result.0)
    };
    if tx.base.data.is_empty() {
        if let TxKind::Call(to) = tx.base.kind {
            if let Ok(account) = state.basic_ref(to) {
                if account.map_or(true, |account| {
                    account.is_empty_code_hash() || account.code_hash().is_zero()
                }) && run(21_000).is_ok_and(|result| result.is_success())
                {
                    return Ok(buffered(21_000, buffer));
                }
            }
        }
    }
    high = high.min(tx.base.gas_limit);
    let result = run(high).map_err(EstimateError::Execution)?;
    let refund = match &result {
        ExecutionResult::Success { gas, .. } => gas.inner_refunded(),
        ExecutionResult::Revert { output, .. } => {
            return Err(EstimateError::Revert(output.clone()))
        }
        ExecutionResult::Halt { reason, .. } => return Err(EstimateError::Halt(reason.clone())),
    };
    let mut used = result.tx_gas_used();
    let mut low = used.saturating_sub(1);
    let optimistic = (used + refund + 2_300) * 64 / 63;
    if optimistic < high {
        let result = run(optimistic).map_err(EstimateError::Execution)?;
        used = result.tx_gas_used();
        update_range(&result, optimistic, &mut high, &mut low)?;
    }
    let mut mid = (used * 3).min(((u128::from(high) + u128::from(low)) / 2) as u64);
    while low + 1 < high {
        if cancelled() {
            return Err(EstimateError::Cancelled);
        }
        if (high - low) as f64 / (high as f64) < 0.015 {
            break;
        }
        match run(mid) {
            Ok(result) => update_range(&result, mid, &mut high, &mut low)?,
            Err(EVMError::Transaction(
                InvalidTransaction::CallerGasLimitMoreThanBlock
                | InvalidTransaction::TxGasLimitGreaterThanCap { .. },
            )) => high = mid,
            Err(EVMError::Transaction(
                InvalidTransaction::CallGasCostMoreThanGasLimit { .. }
                | InvalidTransaction::GasFloorMoreThanGasLimit { .. },
            )) => low = mid,
            Err(error) => return Err(EstimateError::Execution(error)),
        }
        mid = ((u128::from(high) + u128::from(low)) / 2) as u64;
    }
    Ok(buffered(high, buffer))
}

fn update_range<E>(
    result: &ExecutionResult<HaltReason>,
    gas: u64,
    high: &mut u64,
    low: &mut u64,
) -> Result<(), EstimateError<E>> {
    match result {
        ExecutionResult::Success { .. } => *high = gas,
        ExecutionResult::Revert { .. }
        | ExecutionResult::Halt {
            reason: HaltReason::OutOfGas(_) | HaltReason::InvalidFEOpcode,
            ..
        } => *low = gas,
        ExecutionResult::Halt { reason, .. } => return Err(EstimateError::Halt(reason.clone())),
    }
    Ok(())
}
