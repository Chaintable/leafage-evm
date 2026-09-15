//! Port of `execution/monad/staking/staking_contract.cpp` (precompile methods
//! only; the system calls `reward` / `snapshot` / `onEpochChange` are executed
//! by the node and their effect is already part of the state diff).

use super::abi::{encode_address, encode_u256, encode_u64, Decoder, Encoder};
use super::crypto;
use super::error::{Failure, StakingError, StakingResult};
use super::state::{
    AddressFlags, ConsensusView, Delegator, Epochs, KeysPacked, ListNode, RefCountedAccumulator,
    StakingState, ValExecution, Valset, WithdrawalRequest,
};
use crate::monad::hardforks::MON;
use crate::monad::precompile::STAKING_CONTRACT_ADDRESS;
use crate::monad::MonadHardfork;
use revm::context_interface::journaled_state::TransferError;
use revm::context_interface::JournalTr;
use revm::primitives::{b256, Address, Bytes, Log, LogData, B256, U256};

// `staking/util/constants.hpp`
/// `UNIT_BIAS` = 1e36
const UNIT_BIAS: U256 = U256::from_limbs([0xb34b_9f10_0000_0000, 0x00c0_97ce_7bc9_0715, 0, 0]);
/// `limits::dust_threshold()` = 1e9
const DUST_THRESHOLD: U256 = U256::from_limbs([1_000_000_000, 0, 0, 0]);
/// `limits::max_commission()` = 1e18
const MAX_COMMISSION: U256 = MON;
/// `limits::min_external_reward()` = 1e9
const MIN_EXTERNAL_REWARD: U256 = DUST_THRESHOLD;
/// `limits::max_external_reward()` = 1e25
const MAX_EXTERNAL_REWARD: U256 = U256::from_limbs([0x1614_0148_4a00_0000, 0x0008_4595, 0, 0]);
/// `limits::array_pagination()`
const ARRAY_PAGINATION: u32 = 100;
/// `limits::withdrawal_delay()`
const WITHDRAWAL_DELAY: u64 = 1;

/// `limits::min_auth_address_stake()` = 100 000 MON
fn min_auth_address_stake() -> U256 {
    MON * U256::from(100_000u64)
}

// Validator flags.
const VALIDATOR_FLAGS_OK: u64 = 0;
const VALIDATOR_FLAGS_STAKE_TOO_LOW: u64 = 1 << 0;
const VALIDATOR_FLAG_WITHDRAWN: u64 = 1 << 1;

// Event signatures (asserted against the C++ `static_assert`s in tests).
const VALIDATOR_REWARDED: B256 =
    b256!("3a420a01486b6b28d6ae89c51f5c3bde3e0e74eecbb646a0c481ccba3aae3754");
const VALIDATOR_CREATED: B256 =
    b256!("6f8045cd38e512b8f12f6f02947c632e5f25af03aad132890ecf50015d97c1b2");
const VALIDATOR_STATUS_CHANGED: B256 =
    b256!("c95966754e882e03faffaf164883d98986dda088d09471a35f9e55363daf0c53");
const DELEGATE: B256 = b256!("e4d4df1e1827dd28252fd5c3cd7ebccd3da6e0aa31f74c828f3c8542af49d840");
const UNDELEGATE: B256 = b256!("3e53c8b91747e1b72a44894db10f2a45fa632b161fdcdd3a17bd6be5482bac62");
const WITHDRAW: B256 = b256!("63030e4238e1146c63f38f4ac81b2b23c8be28882e68b03f0887e50d0e9bb18f");
const CLAIM_REWARDS: B256 =
    b256!("cb607e6b63c89c95f6ae24ece9fe0e38a7971aa5ed956254f1df47490921727b");
const COMMISSION_CHANGED: B256 =
    b256!("d1698d3454c5b5384b70aaae33f1704af7c7e055f0c75503ba3146dc28995920");

// -- checked math (`core/checked_math.hpp`) ------------------------------------

fn checked_add(x: U256, y: U256) -> StakingResult<U256> {
    x.checked_add(y)
        .ok_or(Failure::Revert(StakingError::Overflow.message()))
}

fn checked_sub(x: U256, y: U256) -> StakingResult<U256> {
    x.checked_sub(y)
        .ok_or(Failure::Revert(StakingError::Underflow.message()))
}

fn checked_mul(x: U256, y: U256) -> StakingResult<U256> {
    x.checked_mul(y)
        .ok_or(Failure::Revert(StakingError::Overflow.message()))
}

fn checked_div(x: U256, y: U256) -> StakingResult<U256> {
    x.checked_div(y)
        .ok_or(Failure::Revert(StakingError::DivisionByZero.message()))
}

fn checked_mul_div(x: U256, y: U256, z: U256) -> StakingResult<U256> {
    checked_div(checked_mul(x, y)?, z)
}

fn calculate_rewards(
    stake: U256,
    current_acc: U256,
    last_checked_acc: U256,
) -> StakingResult<U256> {
    let delta = checked_sub(current_acc, last_checked_acc)?;
    checked_mul_div(delta, stake, UNIT_BIAS)
}

fn function_not_payable(value: U256) -> StakingResult<()> {
    if value.is_zero() {
        Ok(())
    } else {
        Err(StakingError::ValueNonZero.into())
    }
}

fn require_empty(input: &Decoder<'_>) -> StakingResult<()> {
    if input.is_empty() {
        Ok(())
    } else {
        Err(StakingError::InvalidInput.into())
    }
}

fn encode_bool_true() -> Bytes {
    Bytes::from(encode_u64(1).to_vec())
}

// -- linked lists (`LinkedListTrait`) ------------------------------------------

/// A doubly linked list stored in `Delegator::ListNode` slots. Two
/// instantiations exist: validator -> delegator addresses, and delegator ->
/// validator ids.
trait LinkedList {
    type Key: Copy;
    type Ptr: Copy + PartialEq;

    const SENTINEL: Self::Ptr;
    const EMPTY: Self::Ptr;

    fn node<J: JournalTr + ?Sized>(
        state: &StakingState<'_, J>,
        key: Self::Key,
        ptr: Self::Ptr,
    ) -> U256;
    fn prev(node: &ListNode) -> Self::Ptr;
    fn next(node: &ListNode) -> Self::Ptr;
    fn set_prev(node: &mut ListNode, ptr: Self::Ptr);
    fn set_next(node: &mut ListNode, ptr: Self::Ptr);
}

/// `LinkedListTrait<Address, u64_be>`: validators a delegator is delegated to.
struct ValidatorsOfDelegator;

impl LinkedList for ValidatorsOfDelegator {
    type Key = Address;
    type Ptr = u64;

    const SENTINEL: u64 = u64::MAX;
    const EMPTY: u64 = 0;

    fn node<J: JournalTr + ?Sized>(state: &StakingState<'_, J>, key: Address, ptr: u64) -> U256 {
        state.delegator(ptr, key).list_node()
    }
    fn prev(node: &ListNode) -> u64 {
        node.iprev
    }
    fn next(node: &ListNode) -> u64 {
        node.inext
    }
    fn set_prev(node: &mut ListNode, ptr: u64) {
        node.iprev = ptr;
    }
    fn set_next(node: &mut ListNode, ptr: u64) {
        node.inext = ptr;
    }
}

/// `LinkedListTrait<u64_be, Address>`: delegators of a validator.
struct DelegatorsOfValidator;

impl LinkedList for DelegatorsOfValidator {
    type Key = u64;
    type Ptr = Address;

    const SENTINEL: Address = Address::new([0xff; 20]);
    const EMPTY: Address = Address::ZERO;

    fn node<J: JournalTr + ?Sized>(state: &StakingState<'_, J>, key: u64, ptr: Address) -> U256 {
        state.delegator(key, ptr).list_node()
    }
    fn prev(node: &ListNode) -> Address {
        node.aprev
    }
    fn next(node: &ListNode) -> Address {
        node.anext
    }
    fn set_prev(node: &mut ListNode, ptr: Address) {
        node.aprev = ptr;
    }
    fn set_next(node: &mut ListNode, ptr: Address) {
        node.anext = ptr;
    }
}

pub(crate) struct StakingContract<'a, J: ?Sized> {
    state: StakingState<'a, J>,
    hardfork: MonadHardfork,
}

impl<'a, J: JournalTr + ?Sized> StakingContract<'a, J> {
    pub(crate) fn new(journal: &'a mut J, hardfork: MonadHardfork) -> Self {
        Self {
            state: StakingState::new(journal),
            hardfork,
        }
    }

    // -- events -----------------------------------------------------------

    fn emit(&mut self, topics: Vec<B256>, data: Vec<u8>) {
        self.state.emit_log(Log {
            address: STAKING_CONTRACT_ADDRESS,
            data: LogData::new_unchecked(topics, Bytes::from(data)),
        });
    }

    fn emit_validator_rewarded_event(
        &mut self,
        val_id: u64,
        from: Address,
        amount: U256,
    ) -> StakingResult<()> {
        let epoch = self.state.epoch()?;
        self.emit(
            vec![
                VALIDATOR_REWARDED,
                encode_u64(val_id).into(),
                encode_address(from).into(),
            ],
            [encode_u256(amount), encode_u64(epoch)].concat(),
        );
        Ok(())
    }

    fn emit_validator_created_event(
        &mut self,
        val_id: u64,
        auth_delegator: Address,
        commission: U256,
    ) {
        self.emit(
            vec![
                VALIDATOR_CREATED,
                encode_u64(val_id).into(),
                encode_address(auth_delegator).into(),
            ],
            encode_u256(commission).to_vec(),
        );
    }

    fn emit_validator_status_changed_event(&mut self, val_id: u64, flags: u64) {
        self.emit(
            vec![VALIDATOR_STATUS_CHANGED, encode_u64(val_id).into()],
            encode_u64(flags).to_vec(),
        );
    }

    fn emit_delegation_event(
        &mut self,
        val_id: u64,
        delegator: Address,
        amount: U256,
        active_epoch: u64,
    ) {
        self.emit(
            vec![
                DELEGATE,
                encode_u64(val_id).into(),
                encode_address(delegator).into(),
            ],
            [encode_u256(amount), encode_u64(active_epoch)].concat(),
        );
    }

    fn emit_undelegate_event(
        &mut self,
        val_id: u64,
        delegator: Address,
        withdrawal_id: u8,
        amount: U256,
        activation_epoch: u64,
    ) {
        self.emit(
            vec![
                UNDELEGATE,
                encode_u64(val_id).into(),
                encode_address(delegator).into(),
            ],
            [
                encode_u64(u64::from(withdrawal_id)),
                encode_u256(amount),
                encode_u64(activation_epoch),
            ]
            .concat(),
        );
    }

    fn emit_withdraw_event(
        &mut self,
        val_id: u64,
        delegator: Address,
        withdrawal_id: u8,
        amount: U256,
    ) -> StakingResult<()> {
        let withdraw_epoch = self.state.epoch()?;
        self.emit(
            vec![
                WITHDRAW,
                encode_u64(val_id).into(),
                encode_address(delegator).into(),
            ],
            [
                encode_u64(u64::from(withdrawal_id)),
                encode_u256(amount),
                encode_u64(withdraw_epoch),
            ]
            .concat(),
        );
        Ok(())
    }

    fn emit_claim_rewards_event(
        &mut self,
        val_id: u64,
        delegator: Address,
        amount: U256,
    ) -> StakingResult<()> {
        let epoch = self.state.epoch()?;
        self.emit(
            vec![
                CLAIM_REWARDS,
                encode_u64(val_id).into(),
                encode_address(delegator).into(),
            ],
            [encode_u256(amount), encode_u64(epoch)].concat(),
        );
        Ok(())
    }

    fn emit_commission_changed_event(
        &mut self,
        val_id: u64,
        old_commission: U256,
        new_commission: U256,
    ) {
        self.emit(
            vec![COMMISSION_CHANGED, encode_u64(val_id).into()],
            [encode_u256(old_commission), encode_u256(new_commission)].concat(),
        );
    }

    // -- helpers ----------------------------------------------------------

    /// `send_tokens`: `add_to_balance(to)` + `subtract_from_balance(STAKING_CA)`.
    fn send_tokens(&mut self, to: Address, amount: U256) -> StakingResult<()> {
        match self.state.send_tokens(to, amount)? {
            None => Ok(()),
            Some(TransferError::OverflowPayment) => Err(StakingError::Overflow.into()),
            Some(TransferError::OutOfFunds | TransferError::CreateCollision) => {
                Err(StakingError::InternalError.into())
            }
        }
    }

    fn get_activation_epoch(&mut self) -> StakingResult<u64> {
        let epoch = self.state.epoch()?;
        Ok(if self.state.in_epoch_delay_period_set()? {
            epoch.wrapping_add(2)
        } else {
            epoch.wrapping_add(1)
        })
    }

    fn is_epoch_active(&mut self, active_epoch: u64) -> StakingResult<bool> {
        let current_epoch = self.state.epoch()?;
        Ok(active_epoch != 0 && active_epoch <= current_epoch)
    }

    fn increment_accumulator_refcount(&mut self, val_id: u64) -> StakingResult<()> {
        let epoch = self.get_activation_epoch()?;
        let key = self.state.accumulated_reward_per_token(epoch, val_id);
        let mut acc = self.state.load_accumulator(key)?;
        acc.refcount = acc.refcount.wrapping_add(U256::from(1));
        acc.value = self.state.load(
            self.state
                .val_execution(val_id)
                .accumulated_reward_per_token(),
        )?;
        self.state.store_accumulator(key, acc)
    }

    fn decrement_accumulator_refcount(&mut self, epoch: u64, val_id: u64) -> StakingResult<U256> {
        let key = self.state.accumulated_reward_per_token(epoch, val_id);
        let acc = self.state.load_accumulator(key)?;
        if acc.refcount.is_zero() {
            return Ok(U256::ZERO);
        }
        let new_refcount = acc.refcount - U256::from(1);
        if new_refcount.is_zero() {
            self.state.clear::<2>(key)?;
        } else {
            self.state.store_accumulator(
                key,
                RefCountedAccumulator {
                    value: acc.value,
                    refcount: new_refcount,
                },
            )?;
        }
        Ok(acc.value)
    }

    /// Returns `true` when the validator was not in the set yet.
    fn add_to_valset(&mut self, val_id: u64) -> StakingResult<bool> {
        let key = self.state.val_bitset_bucket(val_id);
        let mut set = self.state.load(key)?;
        let mask = U256::from(1) << (val_id & 0xff);
        let inserted = (set & mask).is_zero();
        set |= mask;
        self.state.store(key, set)?;
        Ok(inserted)
    }

    fn val_exists(&mut self, val: ValExecution) -> StakingResult<bool> {
        Ok(self
            .state
            .load_address_flags(val.address_flags())?
            .auth_address
            != Address::ZERO)
    }

    fn val_flags(&mut self, val: ValExecution) -> StakingResult<u64> {
        Ok(self.state.load_address_flags(val.address_flags())?.flags)
    }

    fn val_set_flag(&mut self, val: ValExecution, flag: u64) -> StakingResult<()> {
        let mut af = self.state.load_address_flags(val.address_flags())?;
        af.flags |= flag;
        self.state.store_address_flags(val.address_flags(), af)
    }

    fn val_clear_flag(&mut self, val: ValExecution, flag: u64) -> StakingResult<()> {
        let mut af = self.state.load_address_flags(val.address_flags())?;
        af.flags &= !flag;
        self.state.store_address_flags(val.address_flags(), af)
    }

    /// `Delegator::get_next_epoch_stake` (unchecked wrapping sum).
    fn del_next_epoch_stake(&mut self, del: Delegator) -> StakingResult<U256> {
        let stake = self.state.load(del.stake())?;
        let delta = self.state.load(del.delta_stake())?;
        let next_delta = self.state.load(del.next_delta_stake())?;
        Ok(stake.wrapping_add(delta).wrapping_add(next_delta))
    }

    fn can_promote_delta(&mut self, del: Delegator, epoch: u64) -> StakingResult<bool> {
        let epochs = self.state.load_epochs(del.epochs())?;
        Ok(epochs.delta_epoch == 0 && epochs.next_delta_epoch <= epoch.wrapping_add(1))
    }

    fn promote_delta(&mut self, del: Delegator) -> StakingResult<()> {
        let next_delta_stake = self.state.load(del.next_delta_stake())?;
        self.state.store(del.delta_stake(), next_delta_stake)?;
        self.state.clear::<1>(del.next_delta_stake())?;
        let epochs = self.state.load_epochs(del.epochs())?;
        self.state.store_epochs(
            del.epochs(),
            Epochs {
                delta_epoch: epochs.next_delta_epoch,
                next_delta_epoch: 0,
            },
        )
    }

    fn apply_compound(&mut self, val_id: u64, del: Delegator) -> StakingResult<U256> {
        let delta_epoch = self.state.load_epochs(del.epochs())?.delta_epoch;
        let epoch_acc = self.decrement_accumulator_refcount(delta_epoch, val_id)?;
        let stake = self.state.load(del.stake())?;
        let delta_stake = self.state.load(del.delta_stake())?;
        let acc = self.state.load(del.accumulated_reward_per_token())?;

        let rewards = calculate_rewards(stake, epoch_acc, acc)?;
        self.state
            .store(del.accumulated_reward_per_token(), epoch_acc)?;

        let compounded_stake = checked_add(stake, delta_stake)?;
        self.state.store(del.stake(), compounded_stake)?;

        self.promote_delta(del)?;
        Ok(rewards)
    }

    fn reward_invariant(&mut self, val: ValExecution, rewards: U256) -> StakingResult<()> {
        let unclaimed = self.state.load(val.unclaimed_rewards())?;
        if unclaimed < rewards {
            return Err(StakingError::SolvencyError.into());
        }
        let unclaimed_rewards = checked_sub(unclaimed, rewards)?;
        self.state.store(val.unclaimed_rewards(), unclaimed_rewards)
    }

    fn add_delegator_rewards(&mut self, del: Delegator, rewards: U256) -> StakingResult<()> {
        let new_rewards = checked_add(self.state.load(del.rewards())?, rewards)?;
        self.state.store(del.rewards(), new_rewards)
    }

    fn pull_delegator_up_to_date(&mut self, val_id: u64, del: Delegator) -> StakingResult<()> {
        let epoch = self.state.epoch()?;
        if self.can_promote_delta(del, epoch)? {
            self.promote_delta(del)?;
        }
        let val = self.state.val_execution(val_id);

        let epochs = self.state.load_epochs(del.epochs())?;
        let can_compound = self.is_epoch_active(epochs.delta_epoch)?;
        let can_compound_boundary = self.is_epoch_active(epochs.next_delta_epoch)?;
        if can_compound_boundary {
            if !can_compound {
                return Err(StakingError::CompoundLogicError.into());
            }
            let rewards = self.apply_compound(val_id, del)?;
            self.reward_invariant(val, rewards)?;
            self.add_delegator_rewards(del, rewards)?;
        }
        if can_compound {
            let rewards = self.apply_compound(val_id, del)?;
            self.reward_invariant(val, rewards)?;
            self.add_delegator_rewards(del, rewards)?;
        }
        let del_stake = self.state.load(del.stake())?;
        if del_stake.is_zero() {
            return Ok(());
        }

        let val_acc = self.state.load(val.accumulated_reward_per_token())?;
        let del_acc = self.state.load(del.accumulated_reward_per_token())?;
        let rewards = calculate_rewards(del_stake, val_acc, del_acc)?;
        self.reward_invariant(val, rewards)?;

        self.add_delegator_rewards(del, rewards)?;
        self.state
            .store(del.accumulated_reward_per_token(), val_acc)
    }

    fn apply_reward(
        &mut self,
        val_id: u64,
        from: Address,
        new_rewards: U256,
        active_stake: U256,
    ) -> StakingResult<()> {
        let reward_acc = checked_mul_div(new_rewards, UNIT_BIAS, active_stake)?;

        let val = self.state.val_execution(val_id);
        let acc = checked_add(
            self.state.load(val.accumulated_reward_per_token())?,
            reward_acc,
        )?;
        self.state.store(val.accumulated_reward_per_token(), acc)?;

        let unclaimed_rewards =
            checked_add(self.state.load(val.unclaimed_rewards())?, new_rewards)?;
        self.state
            .store(val.unclaimed_rewards(), unclaimed_rewards)?;

        self.emit_validator_rewarded_event(val_id, from, new_rewards)
    }

    fn delegate(&mut self, val_id: u64, stake: U256, address: Address) -> StakingResult<()> {
        let val = self.state.val_execution(val_id);
        if !self.val_exists(val)? {
            return Err(StakingError::UnknownValidator.into());
        }
        if stake < DUST_THRESHOLD {
            return Err(StakingError::DelegationTooSmall.into());
        }

        let del = self.state.delegator(val_id, address);
        self.pull_delegator_up_to_date(val_id, del)?;

        let active_epoch = self.get_activation_epoch()?;
        let epochs = self.state.load_epochs(del.epochs())?;
        let need_future_accumulator;
        if self.state.in_epoch_delay_period()? {
            // delegation during the boundary activates in epoch + 2
            need_future_accumulator = epochs.next_delta_epoch == 0;
            let delta = checked_add(self.state.load(del.next_delta_stake())?, stake)?;
            self.state.store(del.next_delta_stake(), delta)?;
            self.state.store_epochs(
                del.epochs(),
                Epochs {
                    next_delta_epoch: active_epoch,
                    ..epochs
                },
            )?;
        } else {
            need_future_accumulator = epochs.delta_epoch == 0;
            let delta = checked_add(self.state.load(del.delta_stake())?, stake)?;
            self.state.store(del.delta_stake(), delta)?;
            self.state.store_epochs(
                del.epochs(),
                Epochs {
                    delta_epoch: active_epoch,
                    ..epochs
                },
            )?;
        }

        if need_future_accumulator {
            self.increment_accumulator_refcount(val_id)?;
        }
        self.emit_delegation_event(val_id, address, stake, active_epoch);

        let new_val_stake = checked_add(self.state.load(val.stake())?, stake)?;
        self.state.store(val.stake(), new_val_stake)?;

        let old_flags = self.val_flags(val)?;
        if new_val_stake >= self.hardfork.active_validator_stake() {
            self.val_clear_flag(val, VALIDATOR_FLAGS_STAKE_TOO_LOW)?;
        }
        let auth_address = self
            .state
            .load_address_flags(val.address_flags())?
            .auth_address;
        if auth_address == address && self.del_next_epoch_stake(del)? >= min_auth_address_stake() {
            self.val_clear_flag(val, VALIDATOR_FLAG_WITHDRAWN)?;
        }
        let flags = self.val_flags(val)?;
        if flags != old_flags {
            self.emit_validator_status_changed_event(val_id, flags);
        }

        if flags == VALIDATOR_FLAGS_OK && self.add_to_valset(val_id)? {
            let valset = self.state.valset_execution();
            self.state.valset_push(valset, val_id)?;
        }

        self.linked_list_insert::<DelegatorsOfValidator>(val_id, address)?;
        self.linked_list_insert::<ValidatorsOfDelegator>(address, val_id)
    }

    // -- linked lists -----------------------------------------------------

    fn linked_list_insert<L: LinkedList>(
        &mut self,
        key: L::Key,
        this_ptr: L::Ptr,
    ) -> StakingResult<()> {
        if this_ptr == L::EMPTY || this_ptr == L::SENTINEL {
            return Err(StakingError::InvalidInput.into());
        }
        let this_key = L::node(&self.state, key, this_ptr);
        let mut this_node = self.state.load_list_node(this_key)?;
        if L::prev(&this_node) != L::EMPTY {
            // already in the list
            return Ok(());
        }

        let sentinel_key = L::node(&self.state, key, L::SENTINEL);
        let mut sentinel_node = self.state.load_list_node(sentinel_key)?;
        let next_ptr = L::next(&sentinel_node);

        if next_ptr != L::EMPTY {
            let next_key = L::node(&self.state, key, next_ptr);
            let mut next = self.state.load_list_node(next_key)?;
            L::set_prev(&mut next, this_ptr);
            self.state.store_list_node(next_key, next)?;
        }
        L::set_prev(&mut this_node, L::SENTINEL);
        L::set_next(&mut this_node, next_ptr);
        L::set_next(&mut sentinel_node, this_ptr);

        self.state.store_list_node(this_key, this_node)?;
        self.state.store_list_node(sentinel_key, sentinel_node)
    }

    fn linked_list_remove<L: LinkedList>(
        &mut self,
        key: L::Key,
        this_ptr: L::Ptr,
    ) -> StakingResult<()> {
        if this_ptr == L::EMPTY || this_ptr == L::SENTINEL {
            return Err(StakingError::InvalidListEntry.into());
        }
        let this_key = L::node(&self.state, key, this_ptr);
        let mut this_node = self.state.load_list_node(this_key)?;
        if L::prev(&this_node) == L::EMPTY {
            // not in the list
            return Ok(());
        }

        let prev_ptr = L::prev(&this_node);
        let next_ptr = L::next(&this_node);

        let prev_key = L::node(&self.state, key, prev_ptr);
        let mut prev_node = self.state.load_list_node(prev_key)?;
        L::set_next(&mut prev_node, next_ptr);
        self.state.store_list_node(prev_key, prev_node)?;

        if next_ptr != L::EMPTY {
            let next_key = L::node(&self.state, key, next_ptr);
            let mut next_node = self.state.load_list_node(next_key)?;
            L::set_prev(&mut next_node, prev_ptr);
            self.state.store_list_node(next_key, next_node)?;
        }

        L::set_prev(&mut this_node, L::EMPTY);
        L::set_next(&mut this_node, L::EMPTY);
        self.state.store_list_node(this_key, this_node)
    }

    /// Returns `(done, next_ptr, results)`.
    fn linked_list_traverse<L: LinkedList>(
        &mut self,
        key: L::Key,
        start_ptr: L::Ptr,
        limit: u32,
    ) -> StakingResult<(bool, L::Ptr, Vec<L::Ptr>)> {
        let mut ptr = if start_ptr == L::EMPTY {
            let sentinel_key = L::node(&self.state, key, L::SENTINEL);
            L::next(&self.state.load_list_node(sentinel_key)?)
        } else {
            start_ptr
        };
        let first_key = L::node(&self.state, key, ptr);
        if L::prev(&self.state.load_list_node(first_key)?) == L::EMPTY {
            // bogus pointer, not in list.
            return Ok((true, ptr, Vec::new()));
        }

        let mut results = Vec::new();
        let mut nodes_read = 0u32;
        while ptr != L::EMPTY && nodes_read < limit {
            let node_key = L::node(&self.state, key, ptr);
            let node = self.state.load_list_node(node_key)?;
            results.push(ptr);
            ptr = L::next(&node);
            nodes_read += 1;
        }
        let done = ptr == L::EMPTY;
        Ok((done, ptr, results))
    }

    // -- read only precompiles --------------------------------------------

    pub(crate) fn get_validator(&mut self, input: &[u8], value: U256) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let mut input = Decoder::new(input);
        let val_id = input.u64()?;
        require_empty(&input)?;

        let val = self.state.val_execution(val_id);
        let consensus_view = self.state.consensus_view(val_id);
        let snapshot_view = self.state.snapshot_view(val_id);

        let af = self.state.load_address_flags(val.address_flags())?;
        let mut encoder = Encoder::default();
        encoder.add_address(af.auth_address).add_u64(af.flags);
        for key in [
            val.stake(),
            val.accumulated_reward_per_token(),
            val.commission(),
            val.unclaimed_rewards(),
            consensus_view.stake(),
            consensus_view.commission(),
            snapshot_view.stake(),
            snapshot_view.commission(),
        ] {
            encoder.add_u256(self.state.load(key)?);
        }
        let keys = self.state.load_keys(val.keys())?;
        encoder
            .add_bytes(&keys.secp_pubkey)
            .add_bytes(&keys.bls_pubkey);
        Ok(encoder.finish().into())
    }

    pub(crate) fn get_delegator(&mut self, input: &[u8], value: U256) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let mut input = Decoder::new(input);
        let val_id = input.u64()?;
        let address = input.address()?;
        require_empty(&input)?;

        let del = self.state.delegator(val_id, address);
        self.pull_delegator_up_to_date(val_id, del)?;

        let mut encoder = Encoder::default();
        for key in [
            del.stake(),
            del.accumulated_reward_per_token(),
            del.rewards(),
            del.delta_stake(),
            del.next_delta_stake(),
        ] {
            encoder.add_u256(self.state.load(key)?);
        }
        let epochs = self.state.load_epochs(del.epochs())?;
        encoder
            .add_u64(epochs.delta_epoch)
            .add_u64(epochs.next_delta_epoch);
        Ok(encoder.finish().into())
    }

    fn get_valset(&mut self, input: &[u8], valset: Valset) -> StakingResult<Bytes> {
        let mut input = Decoder::new(input);
        let start_index = input.u32()?;
        require_empty(&input)?;

        let len = self.state.valset_length(valset)?;
        if len > u64::from(u32::MAX) {
            return Err(StakingError::InternalError.into());
        }

        let end = len.min(u64::from(start_index) + u64::from(ARRAY_PAGINATION));
        let mut val_ids = Vec::new();
        let mut i = u64::from(start_index);
        while i < end {
            val_ids.push(self.state.valset_get(valset, i)?);
            i += 1;
        }
        let done = end == len;

        let mut encoder = Encoder::default();
        encoder
            .add_bool(done)
            .add_u64(u64::from(i as u32))
            .add_u64_array(&val_ids);
        Ok(encoder.finish().into())
    }

    pub(crate) fn get_consensus_valset(
        &mut self,
        input: &[u8],
        value: U256,
    ) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let valset = self.state.valset_consensus();
        self.get_valset(input, valset)
    }

    pub(crate) fn get_snapshot_valset(
        &mut self,
        input: &[u8],
        value: U256,
    ) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let valset = self.state.valset_snapshot();
        self.get_valset(input, valset)
    }

    pub(crate) fn get_execution_valset(
        &mut self,
        input: &[u8],
        value: U256,
    ) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let valset = self.state.valset_execution();
        self.get_valset(input, valset)
    }

    pub(crate) fn get_delegations(&mut self, input: &[u8], value: U256) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let mut input = Decoder::new(input);
        let delegator = input.address()?;
        let start_val_id = input.u64()?;
        require_empty(&input)?;

        let (done, next_val_id, val_ids) = self.linked_list_traverse::<ValidatorsOfDelegator>(
            delegator,
            start_val_id,
            self.hardfork.linked_list_pagination(),
        )?;

        let mut encoder = Encoder::default();
        encoder
            .add_bool(done)
            .add_u64(next_val_id)
            .add_u64_array(&val_ids);
        Ok(encoder.finish().into())
    }

    pub(crate) fn get_delegators(&mut self, input: &[u8], value: U256) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let mut input = Decoder::new(input);
        let val_id = input.u64()?;
        let start_delegator = input.address()?;
        require_empty(&input)?;

        let (done, next_delegator, delegators) = self
            .linked_list_traverse::<DelegatorsOfValidator>(
                val_id,
                start_delegator,
                self.hardfork.linked_list_pagination(),
            )?;

        let mut encoder = Encoder::default();
        encoder
            .add_bool(done)
            .add_address(next_delegator)
            .add_address_array(&delegators);
        Ok(encoder.finish().into())
    }

    pub(crate) fn get_epoch(&mut self, _input: &[u8], value: U256) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let epoch = self.state.epoch()?;
        let in_epoch_delay_period = self.state.in_epoch_delay_period()?;
        let mut encoder = Encoder::default();
        encoder.add_u64(epoch).add_bool(in_epoch_delay_period);
        Ok(encoder.finish().into())
    }

    pub(crate) fn get_proposer_val_id(
        &mut self,
        _input: &[u8],
        value: U256,
    ) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let proposer_val_id = self.state.proposer_val_id()?;
        let mut encoder = Encoder::default();
        encoder.add_u64(proposer_val_id);
        Ok(encoder.finish().into())
    }

    pub(crate) fn get_withdrawal_request(
        &mut self,
        input: &[u8],
        value: U256,
    ) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let mut input = Decoder::new(input);
        let val_id = input.u64()?;
        let delegator = input.address()?;
        let withdrawal_id = input.u8()?;
        require_empty(&input)?;

        let key = self
            .state
            .withdrawal_request(val_id, delegator, withdrawal_id);
        let request = self.state.load_withdrawal_request(key)?;

        let mut encoder = Encoder::default();
        encoder
            .add_u256(request.amount)
            .add_u256(request.acc)
            .add_u64(request.epoch);
        Ok(encoder.finish().into())
    }

    pub(crate) fn fallback(&mut self, _input: &[u8], _value: U256) -> StakingResult<Bytes> {
        Err(StakingError::MethodNotSupported.into())
    }

    // -- state changing precompiles ---------------------------------------

    pub(crate) fn add_validator(&mut self, input: &[u8], value: U256) -> StakingResult<Bytes> {
        // compressed secp pubkey + compressed bls pubkey + auth address +
        // signed stake + commission rate
        const MESSAGE_SIZE: usize = 33 + 48 + 20 + 32 + 32;

        let mut input = Decoder::new(input);
        // skip the three tail offsets of the head
        input.u256()?;
        input.u256()?;
        input.u256()?;
        let message = input.bytes_tail::<MESSAGE_SIZE>()?;
        let secp_signature_compressed = input.bytes_tail::<64>()?;
        let bls_signature_compressed = input.bytes_tail::<96>()?;
        require_empty(&input)?;

        let secp_pubkey_compressed: [u8; 33] = message[..33].try_into().unwrap();
        let bls_pubkey_compressed: [u8; 48] = message[33..81].try_into().unwrap();
        let auth_address = Address::from_slice(&message[81..101]);
        let signed_stake = &message[101..133];
        let commission = U256::from_be_slice(&message[133..165]);

        if signed_stake != value.to_be_bytes::<32>() {
            return Err(StakingError::InvalidInput.into());
        }
        let stake = value;
        if stake < min_auth_address_stake() {
            return Err(StakingError::InsufficientStake.into());
        }

        let Some(secp_pubkey) = crypto::secp_pubkey(&secp_pubkey_compressed) else {
            return Err(StakingError::InvalidSecpPubkey.into());
        };
        let Some(secp_signature) = crypto::secp_signature(&secp_signature_compressed) else {
            return Err(StakingError::InvalidSecpSignature.into());
        };
        if !crypto::secp_verify(&secp_pubkey, &secp_signature, &message) {
            return Err(StakingError::SecpSignatureVerificationFailed.into());
        }

        let Some(bls_pubkey) = crypto::bls_pubkey(&bls_pubkey_compressed) else {
            return Err(StakingError::InvalidBlsPubkey.into());
        };
        let Some(bls_signature) = crypto::bls_signature(&bls_signature_compressed) else {
            return Err(StakingError::InvalidBlsSignature.into());
        };
        if !crypto::bls_verify(&bls_pubkey, &bls_signature, &message) {
            return Err(StakingError::BlsSignatureVerificationFailed.into());
        }

        if commission > MAX_COMMISSION {
            return Err(StakingError::CommissionTooHigh.into());
        }

        let secp_eth_address = crypto::address_from_secp_pubkey(&secp_pubkey);
        let bls_eth_address = crypto::address_from_bls_pubkey(&bls_pubkey);
        let val_id_key = self.state.val_id(secp_eth_address);
        let val_id_bls_key = self.state.val_id_bls(bls_eth_address);
        if self.state.has_data::<1>(val_id_key)? || self.state.has_data::<1>(val_id_bls_key)? {
            return Err(StakingError::ValidatorExists.into());
        }

        let val_id = self.state.last_val_id()?.wrapping_add(1);
        self.state.store_u64(val_id_key, val_id)?;
        self.state.store_u64(val_id_bls_key, val_id)?;
        self.state.store_last_val_id(val_id)?;

        let val = self.state.val_execution(val_id);
        self.state.store_keys(
            val.keys(),
            KeysPacked {
                secp_pubkey: secp_pubkey_compressed,
                bls_pubkey: bls_pubkey_compressed,
            },
        )?;
        self.state.store_address_flags(
            val.address_flags(),
            AddressFlags {
                auth_address,
                flags: VALIDATOR_FLAGS_STAKE_TOO_LOW,
            },
        )?;
        self.state.store(val.commission(), commission)?;

        self.emit_validator_created_event(val_id, auth_address, commission);

        self.delegate(val_id, stake, auth_address)?;
        Ok(Bytes::from(encode_u64(val_id).to_vec()))
    }

    pub(crate) fn delegate_call(
        &mut self,
        input: &[u8],
        sender: Address,
        value: U256,
    ) -> StakingResult<Bytes> {
        let mut input = Decoder::new(input);
        let val_id = input.u64()?;
        require_empty(&input)?;

        if !value.is_zero() {
            self.delegate(val_id, value, sender)?;
        }
        Ok(encode_bool_true())
    }

    pub(crate) fn undelegate(
        &mut self,
        input: &[u8],
        sender: Address,
        value: U256,
    ) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let mut input = Decoder::new(input);
        let val_id = input.u64()?;
        let stake = input.u256()?;
        let withdrawal_id = input.u8()?;
        require_empty(&input)?;

        let mut amount = stake;
        if amount.is_zero() {
            return Ok(encode_bool_true());
        }

        let val = self.state.val_execution(val_id);
        if !self.val_exists(val)? {
            return Err(StakingError::UnknownValidator.into());
        }

        let request_key = self.state.withdrawal_request(val_id, sender, withdrawal_id);
        if self.state.has_data::<3>(request_key)? {
            return Err(StakingError::WithdrawalIdExists.into());
        }

        let del = self.state.delegator(val_id, sender);
        self.pull_delegator_up_to_date(val_id, del)?;
        let mut val_stake = self.state.load(val.stake())?;
        let mut del_stake = self.state.load(del.stake())?;

        if del_stake < amount {
            return Err(StakingError::InsufficientStake.into());
        }

        val_stake = checked_sub(val_stake, amount)?;
        del_stake = checked_sub(del_stake, amount)?;
        if del_stake < DUST_THRESHOLD {
            // only dust remains: send the rest with this withdrawal.
            amount = checked_add(amount, del_stake)?;
            val_stake = checked_sub(val_stake, del_stake)?;
            del_stake = U256::ZERO;
        }
        self.state.store(val.stake(), val_stake)?;
        self.state.store(del.stake(), del_stake)?;
        let withdrawal_epoch = self.get_activation_epoch()?;

        let old_flags = self.val_flags(val)?;
        let auth_address = self
            .state
            .load_address_flags(val.address_flags())?
            .auth_address;
        if sender == auth_address && self.del_next_epoch_stake(del)? < min_auth_address_stake() {
            self.val_set_flag(val, VALIDATOR_FLAG_WITHDRAWN)?;
        }
        if val_stake < self.hardfork.active_validator_stake() {
            self.val_set_flag(val, VALIDATOR_FLAGS_STAKE_TOO_LOW)?;
        }
        let flags = self.val_flags(val)?;
        if flags != old_flags {
            self.emit_validator_status_changed_event(val_id, flags);
        }
        self.emit_undelegate_event(val_id, sender, withdrawal_id, amount, withdrawal_epoch);

        // each withdrawal request is an independent delegator whose stake is
        // the amount being withdrawn.
        let acc = self.state.load(del.accumulated_reward_per_token())?;
        self.state.store_withdrawal_request(
            request_key,
            WithdrawalRequest {
                amount,
                acc,
                epoch: withdrawal_epoch,
            },
        )?;
        self.increment_accumulator_refcount(val_id)?;

        if self.state.load(del.stake())?.is_zero() {
            self.state.clear::<1>(del.accumulated_reward_per_token())?;
        }

        if self.del_next_epoch_stake(del)?.is_zero() {
            self.linked_list_remove::<DelegatorsOfValidator>(val_id, sender)?;
            self.linked_list_remove::<ValidatorsOfDelegator>(sender, val_id)?;
        }

        Ok(encode_bool_true())
    }

    pub(crate) fn compound(
        &mut self,
        input: &[u8],
        sender: Address,
        value: U256,
    ) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let mut input = Decoder::new(input);
        let val_id = input.u64()?;
        require_empty(&input)?;

        let del = self.state.delegator(val_id, sender);
        self.pull_delegator_up_to_date(val_id, del)?;
        let rewards = self.state.load(del.rewards())?;
        self.state.clear::<1>(del.rewards())?;

        if !rewards.is_zero() {
            self.emit_claim_rewards_event(val_id, sender, rewards)?;
            self.delegate(val_id, rewards, sender)?;
        }

        Ok(encode_bool_true())
    }

    pub(crate) fn withdraw(
        &mut self,
        input: &[u8],
        sender: Address,
        value: U256,
    ) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let mut input = Decoder::new(input);
        let val_id = input.u64()?;
        let withdrawal_id = input.u8()?;
        require_empty(&input)?;

        let request_key = self.state.withdrawal_request(val_id, sender, withdrawal_id);
        if !self.state.has_data::<3>(request_key)? {
            return Err(StakingError::UnknownWithdrawalId.into());
        }
        let request = self.state.load_withdrawal_request(request_key)?;
        self.state.clear::<3>(request_key)?;

        if !self.is_epoch_active(request.epoch.wrapping_add(WITHDRAWAL_DELAY))? {
            return Err(StakingError::WithdrawalNotReady.into());
        }

        let withdraw_acc = self.decrement_accumulator_refcount(request.epoch, val_id)?;
        let rewards = calculate_rewards(request.amount, withdraw_acc, request.acc)?;
        let val = self.state.val_execution(val_id);
        self.reward_invariant(val, rewards)?;

        let withdrawal_amount = checked_add(request.amount, rewards)?;
        if self.state.balance(STAKING_CONTRACT_ADDRESS)? < withdrawal_amount {
            return Err(StakingError::WithdrawalInsolvent.into());
        }
        self.send_tokens(sender, withdrawal_amount)?;

        self.emit_withdraw_event(val_id, sender, withdrawal_id, withdrawal_amount)?;

        Ok(encode_bool_true())
    }

    pub(crate) fn claim_rewards(
        &mut self,
        input: &[u8],
        sender: Address,
        value: U256,
    ) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let mut input = Decoder::new(input);
        let val_id = input.u64()?;
        require_empty(&input)?;

        let del = self.state.delegator(val_id, sender);
        self.pull_delegator_up_to_date(val_id, del)?;

        let rewards = self.state.load(del.rewards())?;
        if !rewards.is_zero() {
            self.send_tokens(sender, rewards)?;
            self.state.clear::<1>(del.rewards())?;
            self.emit_claim_rewards_event(val_id, sender, rewards)?;
        }

        Ok(encode_bool_true())
    }

    pub(crate) fn change_commission(
        &mut self,
        input: &[u8],
        sender: Address,
        value: U256,
    ) -> StakingResult<Bytes> {
        function_not_payable(value)?;
        let mut input = Decoder::new(input);
        let val_id = input.u64()?;
        let new_commission = input.u256()?;
        require_empty(&input)?;

        let val = self.state.val_execution(val_id);
        if !self.val_exists(val)? {
            return Err(StakingError::UnknownValidator.into());
        }
        let auth_address = self
            .state
            .load_address_flags(val.address_flags())?
            .auth_address;
        if sender != auth_address {
            return Err(StakingError::RequiresAuthAddress.into());
        }
        if new_commission > MAX_COMMISSION {
            return Err(StakingError::CommissionTooHigh.into());
        }

        let old_commission = self.state.load(val.commission())?;
        if old_commission != new_commission {
            self.state.store(val.commission(), new_commission)?;
            self.emit_commission_changed_event(val_id, old_commission, new_commission);
        }

        Ok(encode_bool_true())
    }

    pub(crate) fn external_reward(
        &mut self,
        input: &[u8],
        sender: Address,
        value: U256,
    ) -> StakingResult<Bytes> {
        let external_reward = value;
        let mut input = Decoder::new(input);
        let val_id = input.u64()?;
        require_empty(&input)?;

        let val = self.state.val_execution(val_id);
        if !self.val_exists(val)? {
            return Err(StakingError::UnknownValidator.into());
        }
        let consensus_view: ConsensusView = self.state.this_epoch_view(val_id)?;
        let active_stake = self.state.load(consensus_view.stake())?;
        if active_stake.is_zero() {
            return Err(StakingError::NotInValidatorSet.into());
        }

        if external_reward < MIN_EXTERNAL_REWARD {
            return Err(StakingError::ExternalRewardTooSmall.into());
        }
        if external_reward > MAX_EXTERNAL_REWARD {
            return Err(StakingError::ExternalRewardTooLarge.into());
        }

        self.apply_reward(val_id, sender, external_reward, active_stake)?;

        Ok(encode_bool_true())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::primitives::keccak256;

    #[test]
    fn constants_match_staking_constants_hpp() {
        assert_eq!(UNIT_BIAS, U256::from(10u64).pow(U256::from(36)));
        assert_eq!(DUST_THRESHOLD, U256::from(1_000_000_000u64));
        assert_eq!(MAX_EXTERNAL_REWARD, U256::from(10u64).pow(U256::from(25)));
        assert_eq!(MAX_COMMISSION, U256::from(10u64).pow(U256::from(18)));
        assert_eq!(
            min_auth_address_stake(),
            U256::from(10u64).pow(U256::from(23))
        );
    }

    #[test]
    fn event_signatures_match_cpp_static_asserts() {
        for (sig, hash) in [
            (
                "ValidatorRewarded(uint64,address,uint256,uint64)",
                VALIDATOR_REWARDED,
            ),
            (
                "ValidatorCreated(uint64,address,uint256)",
                VALIDATOR_CREATED,
            ),
            (
                "ValidatorStatusChanged(uint64,uint64)",
                VALIDATOR_STATUS_CHANGED,
            ),
            ("Delegate(uint64,address,uint256,uint64)", DELEGATE),
            (
                "Undelegate(uint64,address,uint8,uint256,uint64)",
                UNDELEGATE,
            ),
            ("Withdraw(uint64,address,uint8,uint256,uint64)", WITHDRAW),
            ("ClaimRewards(uint64,address,uint256,uint64)", CLAIM_REWARDS),
            (
                "CommissionChanged(uint64,uint256,uint256)",
                COMMISSION_CHANGED,
            ),
        ] {
            assert_eq!(keccak256(sig.as_bytes()), hash, "{sig}");
        }
    }

    #[test]
    fn calculate_rewards_uses_unit_bias() {
        // 1 MON stake, accumulator advanced by 0.5e36 -> 0.5 MON
        let stake = MON;
        let rewards = calculate_rewards(stake, UNIT_BIAS / U256::from(2), U256::ZERO).unwrap();
        assert_eq!(rewards, MON / U256::from(2));
        assert_eq!(
            calculate_rewards(stake, U256::ZERO, U256::from(1)),
            Err(Failure::Revert("underflow"))
        );
    }
}
