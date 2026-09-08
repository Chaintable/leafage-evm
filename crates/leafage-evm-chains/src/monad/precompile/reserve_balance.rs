//! Reserve balance precompile at `0x1001`
//! (`execution/monad/reserve_balance/reserve_balance_contract.cpp`, MONAD_NINE).
//!
//! `dippedIntoReserve()` reports whether the transaction has, so far, pushed
//! an account below its reserve balance (`revert_transaction_cached`), in
//! which case the node reverts the whole transaction at the end. The node
//! tracks the predicate incrementally, here it is evaluated on demand over
//! the journal (`reserve_balance::dipped_into_reserve`).

use super::staking::{run_monad_precompile, Failure};
use crate::monad::reserve_balance::dipped_into_reserve;
use crate::monad::MonadContext;
use alloy_evm::Database;
use revm::interpreter::{CallInputs, InterpreterResult};
use revm::primitives::{Bytes, U256};

/// `abi_encode_selector("dippedIntoReserve()")`
const DIPPED_INTO_RESERVE: u32 = 0x3a61584e;
/// warm sload cost
const DIPPED_INTO_RESERVE_OP_COST: u64 = 100;
const FALLBACK_COST: u64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReserveBalanceError {
    MethodNotSupported,
    ValueNonZero,
    InvalidInput,
}

impl ReserveBalanceError {
    const fn message(self) -> &'static str {
        match self {
            Self::MethodNotSupported => "method not supported",
            Self::ValueNonZero => "value is nonzero",
            Self::InvalidInput => "input is invalid",
        }
    }
}

pub(crate) fn run<DB: Database>(
    context: &mut MonadContext<DB>,
    inputs: &CallInputs,
    input: &[u8],
) -> Result<InterpreterResult, String> {
    let (method, cost, input) = if input.len() >= 4
        && u32::from_be_bytes(input[..4].try_into().unwrap()) == DIPPED_INTO_RESERVE
    {
        (Some(()), DIPPED_INTO_RESERVE_OP_COST, &input[4..])
    } else {
        (None, FALLBACK_COST, input)
    };
    // Evaluated up front: `run_monad_precompile` only hands the journal to
    // the method body. The predicate has no side effects.
    let dipped = match method {
        Some(()) if value_and_input_ok(inputs, input) => {
            Some(dipped_into_reserve(context, None).map_err(|e| e.to_string())?)
        }
        _ => None,
    };
    run_monad_precompile(context, inputs, cost, |_journal, _sender, value| {
        let error = match method {
            None => ReserveBalanceError::MethodNotSupported,
            Some(()) if !value.is_zero() => ReserveBalanceError::ValueNonZero,
            Some(()) if !input.is_empty() => ReserveBalanceError::InvalidInput,
            Some(()) => {
                let dipped = dipped.unwrap_or(false);
                return Ok(Bytes::from(
                    U256::from(dipped as u8).to_be_bytes::<32>().to_vec(),
                ));
            }
        };
        Err(Failure::Revert(error.message()))
    })
}

fn value_and_input_ok(inputs: &CallInputs, input: &[u8]) -> bool {
    inputs.value.get().is_zero() && input.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::primitives::keccak256;

    #[test]
    fn selector_matches_signature() {
        let hash = keccak256(b"dippedIntoReserve()");
        assert_eq!(
            u32::from_be_bytes(hash[..4].try_into().unwrap()),
            DIPPED_INTO_RESERVE
        );
    }
}
