//! Staking precompile at `0x1000` (`execution/monad/staking/`).
//!
//! Call semantics follow `check_call_monad_precompile`:
//! * only a plain `CALL` with no flags reaches the contract: `STATICCALL`,
//!   `DELEGATECALL` / `CALLCODE` and EIP-7702 delegation to the address are
//!   rejected with a generic failure that consumes all gas;
//! * gas is a fixed per-method cost from the dispatch table, charged up front;
//! * an error reverts with the error message as raw revert data and leaves no
//!   gas to the caller.

mod abi;
mod contract;
mod crypto;
mod error;
mod state;

use self::contract::StakingContract;
pub(crate) use self::error::Failure;
use crate::monad::MonadHardfork;
use revm::context::ContextTr;
use revm::context_interface::JournalTr;
use revm::interpreter::{CallInputs, CallScheme, Gas, InstructionResult, InterpreterResult};
use revm::primitives::{Address, Bytes, U256};

/// Function selectors (`PrecompileSelector`, asserted in `staking_contract.cpp`).
mod selector {
    pub(super) const ADD_VALIDATOR: u32 = 0xf145204c;
    pub(super) const DELEGATE: u32 = 0x84994fec;
    pub(super) const UNDELEGATE: u32 = 0x5cf41514;
    pub(super) const COMPOUND: u32 = 0xb34fea67;
    pub(super) const WITHDRAW: u32 = 0xaed2ee73;
    pub(super) const CLAIM_REWARDS: u32 = 0xa76e2ca5;
    pub(super) const CHANGE_COMMISSION: u32 = 0x9bdcc3c8;
    pub(super) const EXTERNAL_REWARD: u32 = 0xe4b3303b;
    pub(super) const GET_EPOCH: u32 = 0x757991a8;
    pub(super) const GET_PROPOSER_VAL_ID: u32 = 0xfbacb0be;
    pub(super) const GET_VALIDATOR: u32 = 0x2b6d639a;
    pub(super) const GET_DELEGATOR: u32 = 0x573c1ce0;
    pub(super) const GET_WITHDRAWAL_REQUEST: u32 = 0x56fa2045;
    pub(super) const GET_CONSENSUS_VALIDATOR_SET: u32 = 0xfb29b729;
    pub(super) const GET_SNAPSHOT_VALIDATOR_SET: u32 = 0xde66a368;
    pub(super) const GET_EXECUTION_VALIDATOR_SET: u32 = 0x7cb074df;
    pub(super) const GET_DELEGATIONS: u32 = 0x4fd66050;
    pub(super) const GET_DELEGATORS: u32 = 0xa0843a26;
}

/// Per-method gas (the `*_OP_COST` `static_assert`s in `staking_contract.cpp`).
mod cost {
    pub(super) const ADD_VALIDATOR: u64 = 505_125;
    pub(super) const DELEGATE: u64 = 260_850;
    pub(super) const UNDELEGATE: u64 = 147_750;
    pub(super) const WITHDRAW: u64 = 68_675;
    pub(super) const COMPOUND: u64 = 289_325;
    pub(super) const CLAIM_REWARDS: u64 = 155_375;
    pub(super) const CHANGE_COMMISSION: u64 = 39_475;
    pub(super) const EXTERNAL_REWARD: u64 = 66_575;
    pub(super) const GET_EPOCH: u64 = 200;
    pub(super) const GET_PROPOSER_VAL_ID: u64 = 100;
    pub(super) const GET_VALIDATOR: u64 = 97_200;
    pub(super) const GET_DELEGATOR: u64 = 184_900;
    pub(super) const GET_WITHDRAWAL_REQUEST: u64 = 24_300;
    pub(super) const GET_VALIDATOR_SET: u64 = 814_000;
    pub(super) const LINKED_LIST_GETTER: u64 = 814_000;
    pub(super) const FALLBACK: u64 = 40_000;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    AddValidator,
    Delegate,
    Undelegate,
    Compound,
    Withdraw,
    ClaimRewards,
    ChangeCommission,
    ExternalReward,
    GetEpoch,
    GetProposerValId,
    GetValidator,
    GetDelegator,
    GetWithdrawalRequest,
    GetConsensusValset,
    GetSnapshotValset,
    GetExecutionValset,
    GetDelegations,
    GetDelegators,
    Fallback,
}

/// `StakingContract::precompile_dispatch`. Returns the method, its gas and the
/// input with the selector stripped.
fn dispatch(input: &[u8], hardfork: MonadHardfork) -> (Method, u64, &[u8]) {
    if input.len() < 4 {
        return (Method::Fallback, cost::FALLBACK, input);
    }
    let selector = u32::from_be_bytes(input[..4].try_into().unwrap());
    let rest = &input[4..];
    let (method, gas) = match selector {
        selector::ADD_VALIDATOR => (Method::AddValidator, cost::ADD_VALIDATOR),
        selector::DELEGATE => (Method::Delegate, cost::DELEGATE),
        selector::UNDELEGATE => (Method::Undelegate, cost::UNDELEGATE),
        selector::COMPOUND => (Method::Compound, cost::COMPOUND),
        selector::WITHDRAW => (Method::Withdraw, cost::WITHDRAW),
        selector::CLAIM_REWARDS => (Method::ClaimRewards, cost::CLAIM_REWARDS),
        selector::CHANGE_COMMISSION => (Method::ChangeCommission, cost::CHANGE_COMMISSION),
        selector::EXTERNAL_REWARD => (Method::ExternalReward, cost::EXTERNAL_REWARD),
        selector::GET_EPOCH => (Method::GetEpoch, cost::GET_EPOCH),
        selector::GET_PROPOSER_VAL_ID if hardfork.is_proposer_val_id_enabled() => {
            (Method::GetProposerValId, cost::GET_PROPOSER_VAL_ID)
        }
        selector::GET_VALIDATOR => (Method::GetValidator, cost::GET_VALIDATOR),
        selector::GET_DELEGATOR => (Method::GetDelegator, cost::GET_DELEGATOR),
        selector::GET_WITHDRAWAL_REQUEST => {
            (Method::GetWithdrawalRequest, cost::GET_WITHDRAWAL_REQUEST)
        }
        selector::GET_CONSENSUS_VALIDATOR_SET => {
            (Method::GetConsensusValset, cost::GET_VALIDATOR_SET)
        }
        selector::GET_SNAPSHOT_VALIDATOR_SET => {
            (Method::GetSnapshotValset, cost::GET_VALIDATOR_SET)
        }
        selector::GET_EXECUTION_VALIDATOR_SET => {
            (Method::GetExecutionValset, cost::GET_VALIDATOR_SET)
        }
        selector::GET_DELEGATIONS => (Method::GetDelegations, cost::LINKED_LIST_GETTER),
        selector::GET_DELEGATORS => (Method::GetDelegators, cost::LINKED_LIST_GETTER),
        _ => (Method::Fallback, cost::FALLBACK),
    };
    (method, gas, rest)
}

/// `check_call_monad_precompile` for a stateful Monad precompile.
///
/// `execute` receives the journal, the sender and the call value, and returns
/// the method output or a failure.
pub(crate) fn run_monad_precompile<CTX, F>(
    context: &mut CTX,
    inputs: &CallInputs,
    cost: u64,
    execute: F,
) -> Result<InterpreterResult, String>
where
    CTX: ContextTr,
    F: FnOnce(&mut CTX::Journal, Address, U256) -> Result<Bytes, Failure>,
{
    let gas_limit = inputs.gas_limit;
    // `msg.kind != EVMC_CALL || msg.flags != 0` -> EVMC_REJECTED.
    if inputs.scheme != CallScheme::Call
        || inputs.is_static
        || inputs.target_address != inputs.bytecode_address
    {
        return Ok(InterpreterResult {
            result: InstructionResult::PrecompileError,
            gas: Gas::new_spent(gas_limit),
            output: Bytes::new(),
        });
    }

    if gas_limit < cost {
        return Ok(InterpreterResult {
            result: InstructionResult::PrecompileOOG,
            gas: Gas::new_spent(gas_limit),
            output: Bytes::new(),
        });
    }

    let journal = context.journal_mut();
    journal
        .load_account(inputs.bytecode_address)
        .map_err(|error| error.to_string())?;
    match execute(journal, inputs.caller, inputs.call_value()) {
        Ok(output) => {
            let mut gas = Gas::new(gas_limit);
            // `gas_limit >= cost` was checked above.
            let _ = gas.record_cost(cost);
            Ok(InterpreterResult {
                result: InstructionResult::Return,
                gas,
                output,
            })
        }
        Err(Failure::Revert(message)) => Ok(InterpreterResult {
            result: InstructionResult::Revert,
            gas: Gas::new_spent(gas_limit),
            output: Bytes::from_static(message.as_bytes()),
        }),
        Err(Failure::Fatal(error)) => Err(error),
    }
}

pub(crate) fn run<CTX: ContextTr>(
    context: &mut CTX,
    inputs: &CallInputs,
    input: &[u8],
    hardfork: MonadHardfork,
) -> Result<InterpreterResult, String> {
    let (method, cost, input) = dispatch(input, hardfork);
    run_monad_precompile(context, inputs, cost, |journal, sender, value| {
        let mut contract = StakingContract::new(journal, hardfork);
        match method {
            Method::AddValidator => contract.add_validator(input, value),
            Method::Delegate => contract.delegate_call(input, sender, value),
            Method::Undelegate => contract.undelegate(input, sender, value),
            Method::Compound => contract.compound(input, sender, value),
            Method::Withdraw => contract.withdraw(input, sender, value),
            Method::ClaimRewards => contract.claim_rewards(input, sender, value),
            Method::ChangeCommission => contract.change_commission(input, sender, value),
            Method::ExternalReward => contract.external_reward(input, sender, value),
            Method::GetEpoch => contract.get_epoch(input, value),
            Method::GetProposerValId => contract.get_proposer_val_id(input, value),
            Method::GetValidator => contract.get_validator(input, value),
            Method::GetDelegator => contract.get_delegator(input, value),
            Method::GetWithdrawalRequest => contract.get_withdrawal_request(input, value),
            Method::GetConsensusValset => contract.get_consensus_valset(input, value),
            Method::GetSnapshotValset => contract.get_snapshot_valset(input, value),
            Method::GetExecutionValset => contract.get_execution_valset(input, value),
            Method::GetDelegations => contract.get_delegations(input, value),
            Method::GetDelegators => contract.get_delegators(input, value),
            Method::Fallback => contract.fallback(input, value),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::primitives::keccak256;

    fn selector_of(signature: &str) -> u32 {
        u32::from_be_bytes(keccak256(signature.as_bytes())[..4].try_into().unwrap())
    }

    #[test]
    fn selectors_match_function_signatures() {
        for (signature, expected) in [
            ("addValidator(bytes,bytes,bytes)", selector::ADD_VALIDATOR),
            ("delegate(uint64)", selector::DELEGATE),
            ("undelegate(uint64,uint256,uint8)", selector::UNDELEGATE),
            ("compound(uint64)", selector::COMPOUND),
            ("withdraw(uint64,uint8)", selector::WITHDRAW),
            ("claimRewards(uint64)", selector::CLAIM_REWARDS),
            (
                "changeCommission(uint64,uint256)",
                selector::CHANGE_COMMISSION,
            ),
            ("externalReward(uint64)", selector::EXTERNAL_REWARD),
            ("getEpoch()", selector::GET_EPOCH),
            ("getProposerValId()", selector::GET_PROPOSER_VAL_ID),
            ("getValidator(uint64)", selector::GET_VALIDATOR),
            ("getDelegator(uint64,address)", selector::GET_DELEGATOR),
            (
                "getWithdrawalRequest(uint64,address,uint8)",
                selector::GET_WITHDRAWAL_REQUEST,
            ),
            (
                "getConsensusValidatorSet(uint32)",
                selector::GET_CONSENSUS_VALIDATOR_SET,
            ),
            (
                "getSnapshotValidatorSet(uint32)",
                selector::GET_SNAPSHOT_VALIDATOR_SET,
            ),
            (
                "getExecutionValidatorSet(uint32)",
                selector::GET_EXECUTION_VALIDATOR_SET,
            ),
            ("getDelegations(address,uint64)", selector::GET_DELEGATIONS),
            ("getDelegators(uint64,address)", selector::GET_DELEGATORS),
        ] {
            assert_eq!(selector_of(signature), expected, "{signature}");
        }
    }

    #[test]
    fn dispatch_gates_proposer_val_id_on_monad_five() {
        let input = selector::GET_PROPOSER_VAL_ID.to_be_bytes();
        assert_eq!(
            dispatch(&input, MonadHardfork::MonadFour).0,
            Method::Fallback
        );
        assert_eq!(
            dispatch(&input, MonadHardfork::MonadSix).0,
            Method::GetProposerValId
        );
        assert_eq!(
            dispatch(&[1, 2, 3], MonadHardfork::MonadTen),
            (Method::Fallback, cost::FALLBACK, &[1u8, 2, 3][..])
        );
        let mut input = selector::GET_EPOCH.to_be_bytes().to_vec();
        input.push(9);
        assert_eq!(
            dispatch(&input, MonadHardfork::MonadTen),
            (Method::GetEpoch, cost::GET_EPOCH, &[9u8][..])
        );
    }
}
