//! Reserve balance rule (`execution/monad/reserve_balance.cpp`, MONAD_FOUR).
//!
//! Every EOA (an account without code, or with an EIP-7702 delegation
//! designator) touched by a transaction must end the transaction with at
//! least `min(10 MON, balance before the transaction)`. For the sender the
//! reserve is reduced by the gas fees (`gas_limit * gas_price`) and the sender
//! may dip into its reserve when it is not delegated and is not one of the
//! authorities of the transaction. A violation reverts the whole transaction
//! (`EVMC_MONAD_RESERVE_BALANCE_VIOLATION`, all gas consumed).
//!
//! The node evaluates the rule on the result of the top level message,
//! *before* the message frame is accepted or rejected
//! (`execute_message.cpp`, [`apply_reserve_balance_rule`]); the
//! `dippedIntoReserve()` precompile reports the same predicate at the time of
//! the call. Both share [`dipped_into_reserve`].
//!
//! The node also refuses to let a sender dip into its reserve when it sent a
//! transaction in the same, the parent or the grandparent block. That block
//! context is not available to a simulation and is ignored, like in the RPC
//! executor of the node.

use crate::monad::hardforks::MON;
use crate::monad::{MonadContext, STAKING_CONTRACT_ADDRESS};
use alloy_evm::Database;
use revm::bytecode::Bytecode;
use revm::context::{ContextTr, JournalTr, Transaction};
use revm::context_interface::transaction::AuthorizationTr;
use revm::context_interface::Block;
use revm::handler::{EthFrame, FrameData};
use revm::interpreter::{Gas, InstructionResult, InterpreterAction, InterpreterResult};
use revm::primitives::{Address, Bytes, B256, U256};

/// `monad_default_max_reserve_balance_mon`: 10 MON.
pub(crate) const MAX_RESERVE_BALANCE: U256 = U256::from_limbs([MON.as_limbs()[0] * 10, 0, 0, 0]);

struct Candidate {
    address: Address,
    code_hash: B256,
    original_balance: U256,
    balance: U256,
}

fn is_empty_code_hash(hash: B256) -> bool {
    hash == revm::primitives::KECCAK_EMPTY || hash == B256::ZERO
}

/// `dipped_into_reserve`: does the current state violate the reserve balance
/// of any EOA touched by the transaction?
///
/// `pending_contract` is the address of a top level CREATE whose init code
/// succeeded: the node runs the check after `deploy_contract_code`, so from
/// MONAD_EIGHT (recent code hash) that account is a contract, not an EOA.
pub(crate) fn dipped_into_reserve<DB: Database>(
    ctx: &mut MonadContext<DB>,
    pending_contract: Option<Address>,
) -> Result<bool, DB::Error> {
    let hardfork = ctx.cfg().spec();
    let basefee = ctx.block().basefee() as u128;
    let tx = ctx.tx();
    let sender = tx.caller();
    let gas_fees = U256::from(tx.gas_limit()) * U256::from(tx.effective_gas_price(basefee));
    let sender_is_authority = tx
        .authorization_list()
        .any(|auth| auth.authority() == Some(sender));
    let use_recent_code = hardfork.reserve_check_uses_recent_code();
    let exempt_init_selfdestruct = hardfork.reserve_check_exempts_init_selfdestruct();

    let journal = ctx.journal_mut();
    let transaction_id = journal.inner.transaction_id;
    let candidates: Vec<Candidate> = journal
        .inner
        .state
        .iter()
        .filter(|(address, account)| {
            // the staking contract balance may decrease (withdrawals) and it
            // never sends transactions
            **address != STAKING_CONTRACT_ADDRESS
                && account.transaction_id == transaction_id
                && !(use_recent_code && Some(**address) == pending_contract)
        })
        .filter_map(|(address, account)| {
            let code_hash = if use_recent_code {
                account.info.code_hash
            } else {
                account.original_info.code_hash
            };
            // Contracts that self destruct during init never get a code hash.
            if is_empty_code_hash(code_hash)
                && exempt_init_selfdestruct
                && account.is_selfdestructed()
                && account.is_created()
            {
                return None;
            }
            Some(Candidate {
                address: *address,
                code_hash,
                original_balance: account.original_info.balance,
                balance: account.info.balance,
            })
        })
        .collect();

    for candidate in candidates {
        let is_delegated = if is_empty_code_hash(candidate.code_hash) {
            false
        } else {
            let code = if use_recent_code {
                journal
                    .load_account_with_code(candidate.address)?
                    .info
                    .code
                    .clone()
            } else {
                journal
                    .inner
                    .state
                    .get(&candidate.address)
                    .and_then(|account| account.original_info.code.clone())
            };
            let code = match code {
                Some(code) => code,
                None => journal.db_mut().code_by_hash(candidate.code_hash)?,
            };
            if !is_delegation_designator(&code) {
                // smart contract: not subject to the reserve
                continue;
            }
            true
        };

        let reserve = MAX_RESERVE_BALANCE.min(candidate.original_balance);
        let threshold = if candidate.address == sender {
            // the gas fees alone dip into the reserve
            reserve.checked_sub(gas_fees)
        } else {
            Some(reserve)
        };
        let violated = threshold.is_none_or(|threshold| candidate.balance < threshold);
        if !violated {
            continue;
        }
        if candidate.address == sender {
            // `can_sender_dip_into_reserve`
            if is_delegated || sender_is_authority {
                return Ok(true);
            }
        } else {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_delegation_designator(code: &Bytecode) -> bool {
    code.eip7702_address().is_some()
}

/// `execute_message.cpp` depth 0: `revert_transaction` runs on the result of
/// the top level message before `post_call` accepts or rejects the frame.
/// A violation turns the result into `EVMC_MONAD_RESERVE_BALANCE_VIOLATION`
/// with all gas consumed; the frame is then rejected like any other failure,
/// which keeps everything recorded outside the frame (sender nonce of a
/// CREATE, EIP-7702 authorizations, gas payment).
///
/// Returns whether the rule was violated.
pub(crate) fn apply_reserve_balance_rule<DB: Database>(
    ctx: &mut MonadContext<DB>,
    frame: &EthFrame,
    action: &mut InterpreterAction,
) -> Result<bool, DB::Error> {
    if frame.depth != 0 || !ctx.cfg().spec().is_reserve_balance_check_enabled() {
        return Ok(false);
    }
    let InterpreterAction::Return(result) = action else {
        return Ok(false);
    };
    let pending_contract = match &frame.data {
        FrameData::Create(create) if result.result.is_ok() && !result.output.is_empty() => {
            Some(create.created_address)
        }
        _ => None,
    };
    reject_on_reserve_violation(ctx, pending_contract, result)
}

/// Turns `result` into the reserve balance violation outcome (failure, all
/// gas consumed, no output) when [`dipped_into_reserve`] holds.
pub(crate) fn reject_on_reserve_violation<DB: Database>(
    ctx: &mut MonadContext<DB>,
    pending_contract: Option<Address>,
    result: &mut InterpreterResult,
) -> Result<bool, DB::Error> {
    if !dipped_into_reserve(ctx, pending_contract)? {
        return Ok(false);
    }
    result.result = InstructionResult::OutOfFunds;
    result.gas = Gas::new_spent(result.gas.limit());
    result.output = Bytes::new();
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_reserve_is_ten_mon() {
        assert_eq!(MAX_RESERVE_BALANCE, U256::from(10u128 * 10u128.pow(18)));
    }
}
