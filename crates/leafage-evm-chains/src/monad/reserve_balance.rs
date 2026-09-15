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
use crate::monad::{MonadContext, MonadHardfork, STAKING_CONTRACT_ADDRESS};
use alloy_evm::Database;
use leafage_evm_types::CfgEnv;
use revm::bytecode::Bytecode;
use revm::context::{Cfg, ContextTr, JournalTr, Transaction};
use revm::context_interface::transaction::AuthorizationTr;
use revm::context_interface::Block;
use revm::handler::{EthFrame, FrameData};
use revm::interpreter::{Gas, InstructionResult, InterpreterAction, InterpreterResult};
use revm::primitives::hardfork::SpecId;
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
pub(crate) fn dipped_into_reserve<DB: Database>(
    ctx: &mut MonadContext<DB>,
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
            **address != STAKING_CONTRACT_ADDRESS && account.transaction_id == transaction_id
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

/// Outcome of [`apply_reserve_balance_rule`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReserveRuleOutcome {
    /// Not the top level message, rule inactive, or no violation.
    Passed,
    /// The result was turned into the reserve balance violation.
    Violated,
    /// Top level CREATE whose init code succeeded: the node runs the check
    /// after `deploy_contract_code`, so it has to wait until revm's
    /// `return_create` deployed (or refused) the code, see
    /// [`apply_reserve_balance_rule_after_create`].
    CheckAfterDeploy,
}

/// `execute_message.cpp` depth 0: `revert_transaction` runs on the result of
/// the top level message before `post_call` accepts or rejects the frame.
/// A violation turns the result into `EVMC_MONAD_RESERVE_BALANCE_VIOLATION`
/// with all gas consumed; the frame is then rejected like any other failure,
/// which keeps everything recorded outside the frame (sender nonce of a
/// CREATE, EIP-7702 authorizations, gas payment).
pub(crate) fn apply_reserve_balance_rule<DB: Database>(
    ctx: &mut MonadContext<DB>,
    frame: &EthFrame,
    action: &mut InterpreterAction,
) -> Result<ReserveRuleOutcome, DB::Error> {
    if frame.depth != 0 || !ctx.cfg().spec().is_reserve_balance_check_enabled() {
        return Ok(ReserveRuleOutcome::Passed);
    }
    let InterpreterAction::Return(result) = action else {
        return Ok(ReserveRuleOutcome::Passed);
    };
    if matches!(frame.data, FrameData::Create(_))
        && result.result.is_ok()
        && !deploy_would_fail(ctx.cfg(), result)
    {
        return Ok(ReserveRuleOutcome::CheckAfterDeploy);
    }
    Ok(if reject_on_reserve_violation(ctx, result)? {
        ReserveRuleOutcome::Violated
    } else {
        ReserveRuleOutcome::Passed
    })
}

/// Will revm's `return_create` refuse the returned code (EIP-3541 prefix,
/// EIP-170 size, code deposit gas)? Then `deploy_contract_code` fails in the
/// node as well and the reserve check runs on the un-reverted state, with
/// the address still an EOA.
fn deploy_would_fail(cfg: &CfgEnv<MonadHardfork>, result: &InterpreterResult) -> bool {
    let spec: SpecId = (*cfg.spec()).into();
    let output = &result.output;
    (!cfg.is_eip3541_disabled()
        && spec.is_enabled_in(SpecId::LONDON)
        && output.first() == Some(&0xEF))
        || (spec.is_enabled_in(SpecId::SPURIOUS_DRAGON) && output.len() > cfg.max_code_size())
        || result.gas.remaining() < cfg.gas_params().code_deposit_cost(output.len())
}

/// Second half of [`apply_reserve_balance_rule`] for a top level CREATE whose
/// init code succeeded, run after revm settled the frame: with the code
/// deployed the new account is a contract (MONAD_EIGHT+, recent code hash),
/// when `return_create` refused the code the account stays an EOA like in
/// `deploy_contract_code`. A violation of a deployed contract reverts the
/// frame that revm already committed.
pub(crate) fn apply_reserve_balance_rule_after_create<DB: Database>(
    ctx: &mut MonadContext<DB>,
    frame: &EthFrame,
    result: &mut InterpreterResult,
) -> Result<bool, DB::Error> {
    let deployed = result.result.is_ok();
    if !reject_on_reserve_violation(ctx, result)? {
        return Ok(false);
    }
    if deployed {
        ctx.journal_mut().checkpoint_revert(frame.checkpoint);
    }
    Ok(true)
}

/// Turns `result` into the reserve balance violation outcome (failure, all
/// gas consumed, no output) when [`dipped_into_reserve`] holds.
pub(crate) fn reject_on_reserve_violation<DB: Database>(
    ctx: &mut MonadContext<DB>,
    result: &mut InterpreterResult,
) -> Result<bool, DB::Error> {
    if !dipped_into_reserve(ctx)? {
        return Ok(false);
    }
    mark_reserve_violation(result);
    Ok(true)
}

/// `EVMC_MONAD_RESERVE_BALANCE_VIOLATION`: failure, all gas consumed, no
/// output.
pub(crate) fn mark_reserve_violation(result: &mut InterpreterResult) {
    result.result = InstructionResult::OutOfFunds;
    result.gas = Gas::new_spent(result.gas.limit());
    result.output = Bytes::new();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_reserve_is_ten_mon() {
        assert_eq!(MAX_RESERVE_BALANCE, U256::from(10u128 * 10u128.pow(18)));
    }
}
