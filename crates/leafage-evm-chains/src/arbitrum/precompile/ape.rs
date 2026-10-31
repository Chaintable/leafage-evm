//! ApeChain native yield inside one EVM execution.
//!
//! Account balances enter revm already resolved (`leafage_evm_storage::ApeYieldStorage`)
//! and revm moves them like any balance. The raw yield fields live in the
//! storage of [`APE_YIELD_ADDRESS`] and are only needed by the ApeChain
//! precompile methods and the CREATE / SELFDESTRUCT hooks. Nitro updates those
//! fields on every balance change; here an account whose current balance
//! differs from the balance its stored fields give has a pending `SetBalance`,
//! replayed by [`YieldState::sync`]. Nitro's split depends only on the final
//! balance and debt, and `adjustDebt` leaves the balance unchanged, so one
//! replay gives the fields nitro reaches step by step. Replays happen before
//! the fields are read or the yield mode changes, and at the end of every
//! transaction for all the accounts it touched.
//!
//! The share price is the one the block started with: an `ArbOwner.setSharePrice`
//! in the same simulation is visible to `getSharePrice` but does not re-price
//! balances revm has already loaded.

use super::registry::{ArbitrumPrecompile, ArbitrumPrecompilePurity};
use super::state::ArbStorage;
use super::util::{dispatch, finish_call};
use super::{ArbPrecompileInput, ArbitrumContext};
use crate::arbitrum::arbos_state;
use crate::arbitrum::evm::ArbResourceKind;
use crate::arbitrum::precompile::result::{PrecompileError, PrecompileResult};
use alloy::primitives::{Address, U256};
use alloy::sol_types::SolInterface;
use leafage_evm_types::ape_yield::{
    ape_shares_and_remainder, ape_yield_slot, ApeYieldFields, APE_APY_OFFSET,
    APE_BASE_SHARE_PRICE_OFFSET, APE_BASE_SHARE_TIME_OFFSET, APE_SHARE_COUNT_OFFSET,
    APE_SHARE_PRICE_OFFSET, APE_YIELD_ADDRESS, APE_YIELD_AUTOMATIC, APE_YIELD_DELEGATED,
    APE_YIELD_DISABLED, APE_YIELD_SLOT_COUNT,
};
use revm::context::{Block, ContextTr, JournalTr};
use revm::primitives::KECCAK_EMPTY;
use revm::Database;
use std::collections::{btree_map, BTreeMap};

/// A signed change of an account's debt (nitro passes a two's-complement uint256).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Delta {
    Add(U256),
    Sub(U256),
}

/// The yield fields of the accounts touched by one hook or precompile call.
pub(super) struct YieldState {
    share_price: U256,
    accounts: BTreeMap<Address, Entry>,
    /// Change of ArbOS `shareCount`, as a two's-complement word.
    share_delta: U256,
}

struct Entry {
    stored: ApeYieldFields,
    fields: ApeYieldFields,
}

fn share_price_slot() -> U256 {
    arbos_state::slot_at(&[], APE_SHARE_PRICE_OFFSET)
}

pub(super) fn share_count_slot() -> U256 {
    arbos_state::slot_at(&[], APE_SHARE_COUNT_OFFSET)
}

impl YieldState {
    pub(super) fn new<DB: Database>(ctx: &mut ArbitrumContext<DB>) -> Result<Self, DB::Error> {
        let share_price = ctx
            .db_mut()
            .storage(arbos_state::ARBOS_STATE_ADDRESS, share_price_slot())?;
        Ok(Self {
            share_price,
            accounts: BTreeMap::new(),
            share_delta: U256::ZERO,
        })
    }

    pub(super) fn share_delta(&self) -> U256 {
        self.share_delta
    }

    fn entry<DB: Database>(
        &mut self,
        ctx: &mut ArbitrumContext<DB>,
        address: Address,
    ) -> Result<&mut Entry, DB::Error> {
        let vacant = match self.accounts.entry(address) {
            btree_map::Entry::Occupied(entry) => return Ok(entry.into_mut()),
            btree_map::Entry::Vacant(vacant) => vacant,
        };
        let hash = alloy::primitives::keccak256(address);
        ctx.journal_mut().load_account(APE_YIELD_ADDRESS)?;
        let mut slots = [U256::ZERO; APE_YIELD_SLOT_COUNT];
        for (i, slot) in slots.iter_mut().enumerate() {
            *slot = ctx
                .journal_mut()
                .sload(APE_YIELD_ADDRESS, ape_yield_slot(hash, i))?
                .data;
        }
        let stored = ApeYieldFields::from_slots(slots);
        Ok(vacant.insert(Entry {
            stored,
            fields: stored,
        }))
    }

    /// The account's yield fields as replayed so far.
    pub(super) fn fields<DB: Database>(
        &mut self,
        ctx: &mut ArbitrumContext<DB>,
        address: Address,
    ) -> Result<ApeYieldFields, DB::Error> {
        Ok(self.entry(ctx, address)?.fields)
    }

    fn set_fields<DB: Database>(
        &mut self,
        ctx: &mut ArbitrumContext<DB>,
        address: Address,
        fields: ApeYieldFields,
    ) -> Result<(), DB::Error> {
        self.entry(ctx, address)?.fields = fields;
        Ok(())
    }

    fn adjust_share_count(&mut self, pre: U256, post: U256) {
        self.share_delta = self.share_delta.wrapping_add(post).wrapping_sub(pre);
    }

    /// The account's balance in the journal, else in the database.
    fn current_balance<DB: Database>(
        ctx: &mut ArbitrumContext<DB>,
        address: Address,
    ) -> Result<U256, DB::Error> {
        if let Some(account) = ctx.journal().state.get(&address) {
            return Ok(account.info.balance);
        }
        Ok(ctx
            .db_mut()
            .basic(address)?
            .map(|info| info.balance)
            .unwrap_or_default())
    }

    /// Nitro `stateObject.SetBalance`.
    fn set_balance<DB: Database>(
        &mut self,
        ctx: &mut ArbitrumContext<DB>,
        address: Address,
        balance: U256,
    ) -> Result<(), DB::Error> {
        let mut fields = self.fields(ctx, address)?;
        let amount = balance.wrapping_add(fields.debt);
        let (fixed, shares) = match fields.flags {
            APE_YIELD_AUTOMATIC => {
                let (shares, fixed) = ape_shares_and_remainder(self.share_price, amount);
                (fixed, shares)
            }
            APE_YIELD_DELEGATED | APE_YIELD_DISABLED => {
                if fields.flags == APE_YIELD_DELEGATED && amount != fields.fixed {
                    let delta = if amount > fields.fixed {
                        Delta::Add(amount - fields.fixed)
                    } else {
                        Delta::Sub(fields.fixed - amount)
                    };
                    self.adjust_debt(ctx, fields.delegate, delta)?;
                }
                (amount, U256::ZERO)
            }
            // Nitro has no case for other flags and leaves the fields unset.
            _ => (fields.fixed, fields.shares),
        };
        self.adjust_share_count(fields.shares, shares);
        fields.fixed = fixed;
        fields.shares = shares;
        self.set_fields(ctx, address, fields)
    }

    /// Nitro `stateObject.adjustDebt`: the delegate keeps its balance; the
    /// delegated amount moves into both `fixed` and `debt`.
    fn adjust_debt<DB: Database>(
        &mut self,
        ctx: &mut ArbitrumContext<DB>,
        address: Address,
        delta: Delta,
    ) -> Result<(), DB::Error> {
        let mut fields = self.fields(ctx, address)?;
        // Nitro clamps a delta that would take debt below zero.
        let delta = match delta {
            Delta::Sub(value) => Delta::Sub(value.min(fields.debt)),
            add => add,
        };
        // Nitro adds the delta as a uint256. Under automatic yield `fixed` is
        // only the remainder below one share and may wrap; re-splitting
        // `shares * price + fixed` takes the delta out of the shares.
        fields.fixed = match delta {
            Delta::Add(value) => fields.fixed.wrapping_add(value),
            Delta::Sub(value) if fields.flags == APE_YIELD_AUTOMATIC => {
                fields.fixed.wrapping_sub(value)
            }
            Delta::Sub(value) => fields.fixed.saturating_sub(value),
        };
        if fields.flags == APE_YIELD_AUTOMATIC {
            let gross = fields
                .shares
                .wrapping_mul(self.share_price)
                .wrapping_add(fields.fixed);
            let (shares, fixed) = ape_shares_and_remainder(self.share_price, gross);
            self.adjust_share_count(fields.shares, shares);
            fields.shares = shares;
            fields.fixed = fixed;
        }
        fields.debt = match delta {
            Delta::Add(value) => fields.debt.wrapping_add(value),
            Delta::Sub(value) => fields.debt - value,
        };
        self.set_fields(ctx, address, fields)
    }

    /// Nitro `stateObject.SetFlags`.
    pub(super) fn set_flags<DB: Database>(
        &mut self,
        ctx: &mut ArbitrumContext<DB>,
        address: Address,
        flags: u8,
        delegate: Option<Address>,
    ) -> Result<(), DB::Error> {
        let self_destructed = ctx
            .journal()
            .state
            .get(&address)
            .is_some_and(|account| account.is_selfdestructed());
        if self_destructed && flags != APE_YIELD_DISABLED {
            return Ok(());
        }
        if flags == APE_YIELD_DELEGATED && delegate.is_none_or(|delegate| delegate == address) {
            return Ok(());
        }
        let mut fields = self.fields(ctx, address)?;
        if flags == fields.flags
            && (flags != APE_YIELD_DELEGATED || delegate == Some(fields.delegate))
        {
            return Ok(());
        }

        // The gross value under the old flag.
        let value = match fields.flags {
            APE_YIELD_AUTOMATIC => fields.gross(self.share_price),
            APE_YIELD_DELEGATED => {
                let balance = fields.fixed.saturating_sub(fields.debt);
                if !balance.is_zero() {
                    self.adjust_debt(ctx, fields.delegate, Delta::Sub(balance))?;
                }
                fields.fixed
            }
            _ => fields.fixed,
        };

        // Re-split under the new flag.
        let (fixed, shares) = match flags {
            APE_YIELD_AUTOMATIC => {
                let (shares, fixed) = ape_shares_and_remainder(self.share_price, value);
                fields.delegate = Address::ZERO;
                (fixed, shares)
            }
            APE_YIELD_DELEGATED => {
                let delegate = delegate.expect("checked above");
                fields.delegate = delegate;
                let balance = value.saturating_sub(fields.debt);
                if !balance.is_zero() {
                    self.adjust_debt(ctx, delegate, Delta::Add(balance))?;
                }
                (value, U256::ZERO)
            }
            _ => {
                fields.delegate = Address::ZERO;
                (value, U256::ZERO)
            }
        };
        self.adjust_share_count(fields.shares, shares);
        fields.flags = flags;
        fields.fixed = fixed;
        fields.shares = shares;
        self.set_fields(ctx, address, fields)
    }

    /// Replays the account's pending `SetBalance`, if any.
    pub(super) fn sync<DB: Database>(
        &mut self,
        ctx: &mut ArbitrumContext<DB>,
        address: Address,
    ) -> Result<(), DB::Error> {
        if address == APE_YIELD_ADDRESS {
            return Ok(());
        }
        let current = Self::current_balance(ctx, address)?;
        if self.fields(ctx, address)?.balance(self.share_price) != current {
            self.set_balance(ctx, address, current)?;
        }
        Ok(())
    }

    /// Replays the pending `SetBalance` of every account in the journal.
    pub(super) fn apply_pending<DB: Database>(
        &mut self,
        ctx: &mut ArbitrumContext<DB>,
    ) -> Result<(), DB::Error> {
        let addresses: Vec<Address> = ctx.journal().state.keys().copied().collect();
        for address in addresses {
            self.sync(ctx, address)?;
        }
        Ok(())
    }

    /// Nitro `StateDB.Empty`, with the yield fields counted.
    pub(super) fn is_empty<DB: Database>(
        &mut self,
        ctx: &mut ArbitrumContext<DB>,
        address: Address,
    ) -> Result<bool, DB::Error> {
        if !self.fields(ctx, address)?.is_empty() {
            return Ok(false);
        }
        let (nonce, code_hash) = match ctx.journal().state.get(&address) {
            Some(account) => (account.info.nonce, account.info.code_hash),
            None => ctx
                .db_mut()
                .basic(address)?
                .map(|info| (info.nonce, info.code_hash))
                .unwrap_or((0, KECCAK_EMPTY)),
        };
        Ok(nonce == 0 && (code_hash == KECCAK_EMPTY || code_hash.is_zero()))
    }

    /// Writes the changed yield fields and `shareCount` to the journal.
    pub(super) fn write<DB: Database>(
        self,
        ctx: &mut ArbitrumContext<DB>,
    ) -> Result<(), DB::Error> {
        let mut touched = false;
        for (address, entry) in &self.accounts {
            if entry.fields == entry.stored {
                continue;
            }
            let hash = alloy::primitives::keccak256(address);
            let (old, new) = (entry.stored.to_slots(), entry.fields.to_slots());
            for i in 0..APE_YIELD_SLOT_COUNT {
                if old[i] != new[i] {
                    ctx.journal_mut()
                        .sstore(APE_YIELD_ADDRESS, ape_yield_slot(hash, i), new[i])?;
                    touched = true;
                }
            }
        }
        if touched {
            ctx.journal_mut().touch_account(APE_YIELD_ADDRESS);
        }
        if !self.share_delta.is_zero() {
            let journal = ctx.journal_mut();
            journal.load_account(arbos_state::ARBOS_STATE_ADDRESS)?;
            let count = journal
                .sload(arbos_state::ARBOS_STATE_ADDRESS, share_count_slot())?
                .data;
            journal.sstore(
                arbos_state::ARBOS_STATE_ADDRESS,
                share_count_slot(),
                count.wrapping_add(self.share_delta),
            )?;
            journal.touch_account(arbos_state::ARBOS_STATE_ADDRESS);
        }
        Ok(())
    }
}

/// Nitro `evm.create`: contracts start with yield disabled, after the value
/// transfer.
pub(crate) fn on_create<DB: Database>(
    ctx: &mut ArbitrumContext<DB>,
    address: Address,
) -> Result<(), DB::Error> {
    let mut state = YieldState::new(ctx)?;
    state.sync(ctx, address)?;
    state.set_flags(ctx, address, APE_YIELD_DISABLED, None)?;
    state.write(ctx)
}

/// Nitro `StateDB.SelfDestruct` for a contract created in this transaction:
/// unwind the yield mode, drop the debt, clear the balance.
pub(crate) fn on_selfdestruct<DB: Database>(
    ctx: &mut ArbitrumContext<DB>,
    address: Address,
) -> Result<(), DB::Error> {
    let mut state = YieldState::new(ctx)?;
    // The journal balance is already 0, as after nitro's `SubBalance`.
    state.sync(ctx, address)?;
    state.set_flags(ctx, address, APE_YIELD_DISABLED, None)?;
    let mut fields = state.fields(ctx, address)?;
    fields.debt = U256::ZERO;
    state.set_fields(ctx, address, fields)?;
    state.set_balance(ctx, address, U256::ZERO)?;
    state.write(ctx)
}
/// End of a transaction: the yield fields of every account it touched are
/// brought up to date and written, so the next transaction of a simulation
/// starts from fields that agree with the balances. An account destroyed in
/// it leaves no yield fields, as the writer's state diff zeroes them.
pub(crate) fn on_tx_end<DB: Database>(ctx: &mut ArbitrumContext<DB>) -> Result<(), DB::Error> {
    let (live, destroyed): (Vec<_>, Vec<_>) = ctx
        .journal()
        .state
        .iter()
        .filter(|(address, _)| **address != APE_YIELD_ADDRESS)
        .map(|(address, account)| (*address, account.is_selfdestructed()))
        .partition(|(_, destroyed)| !destroyed);
    let mut state = YieldState::new(ctx)?;
    for (address, _) in live {
        state.sync(ctx, address)?;
    }
    state.write(ctx)?;

    let destroyed: Vec<Address> = destroyed.into_iter().map(|(address, _)| address).collect();
    let mut touched = false;
    for address in destroyed {
        let hash = alloy::primitives::keccak256(address);
        ctx.journal_mut().load_account(APE_YIELD_ADDRESS)?;
        for i in 0..APE_YIELD_SLOT_COUNT {
            let slot = ape_yield_slot(hash, i);
            if !ctx
                .journal_mut()
                .sload(APE_YIELD_ADDRESS, slot)?
                .data
                .is_zero()
            {
                ctx.journal_mut()
                    .sstore(APE_YIELD_ADDRESS, slot, U256::ZERO)?;
                touched = true;
            }
        }
    }
    if touched {
        ctx.journal_mut().touch_account(APE_YIELD_ADDRESS);
    }
    Ok(())
}

alloy::sol! {
    /// ApeChain additions to ArbInfo (nitro-precompile-interfaces `ArbInfo.sol`).
    interface IApeArbInfo {
        function getBalanceValues(address account) external view returns (uint256, uint256, uint256);
        function getYieldConfiguration(address account) external view returns (uint8);
        function getDelegate(address account) external view returns (address);
        function configureAutomaticYield() external;
        function configureVoidYield() external;
        function configureDelegateYield(address account) external;
    }

    /// ApeChain additions to ArbOwnerPublic.
    interface IApeArbOwnerPublic {
        function getSharePrice() external view returns (uint64);
        function getShareCount() external view returns (uint256);
        function getApy() external view returns (uint64);
    }

    /// ApeChain additions to ArbOwner.
    interface IApeArbOwner {
        function setSharePrice(uint64 sharePrice) external;
        function setApy(uint64 apy) external;
    }
}

/// `params.BalanceGasEIP1884`.
const BALANCE_VALUES_GAS: u64 = 700;
/// `2 * params.ColdAccountAccessCostEIP2929`.
const CONFIGURE_DELEGATE_YIELD_GAS: u64 = 2 * 2_600;
/// `params.CallNewAccountGas`.
const CALL_NEW_ACCOUNT_GAS: u64 = 25_000;
/// `arbosState.MaxApy`: 1000%.
const MAX_APY: u64 = 1_000_000_000_000;

/// Whether `data` calls a method of `T`.
pub(super) fn is_call<T: SolInterface>(data: &[u8]) -> bool {
    data.split_first_chunk::<4>()
        .is_some_and(|(selector, _)| T::type_check(*selector).is_ok())
}

/// Purity of the ApeChain ArbInfo / ArbOwnerPublic methods, when `data` calls one.
pub(super) fn purity(
    precompile: ArbitrumPrecompile,
    data: &[u8],
) -> Option<ArbitrumPrecompilePurity> {
    use ArbitrumPrecompilePurity::{View, Write};
    match precompile {
        ArbitrumPrecompile::ArbInfo => {
            IApeArbInfo::IApeArbInfoCalls::abi_decode(data)
                .ok()
                .map(|call| match call {
                    IApeArbInfo::IApeArbInfoCalls::configureAutomaticYield(_)
                    | IApeArbInfo::IApeArbInfoCalls::configureVoidYield(_)
                    | IApeArbInfo::IApeArbInfoCalls::configureDelegateYield(_) => Write,
                    _ => View,
                })
        }
        ArbitrumPrecompile::ArbOwnerPublic => {
            IApeArbOwnerPublic::IApeArbOwnerPublicCalls::abi_decode(data)
                .ok()
                .map(|_| View)
        }
        _ => None,
    }
}

fn fatal<E: std::fmt::Debug>(error: E) -> PrecompileError {
    PrecompileError::Fatal(format!("{error:?}"))
}

/// Burns `gas` as computation, like nitro's `c.Burn(ResourceKindComputation, ..)`.
fn burn<DB: Database>(
    ctx: &mut ArbitrumContext<DB>,
    gas_used: &mut u64,
    gas_limit: u64,
    gas: u64,
) -> Result<(), PrecompileError> {
    *gas_used = gas_used.saturating_add(gas);
    if *gas_used > gas_limit {
        return Err(PrecompileError::OutOfGas);
    }
    ctx.chain_mut()
        .record_multi_gas(ArbResourceKind::Computation, gas);
    Ok(())
}

/// The ApeChain ArbInfo methods.
pub(super) fn run_arb_info<DB: Database>(
    input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
) -> PrecompileResult {
    let gas_limit = input.gas;
    let caller = input.caller;
    let ctx = input.context;
    dispatch::<IApeArbInfo::IApeArbInfoCalls>(input.data, gas_limit, |call, initial_gas| {
        let mut gas_used = initial_gas;
        let mut state = YieldState::new(ctx).map_err(fatal)?;
        match call {
            IApeArbInfo::IApeArbInfoCalls::getBalanceValues(call) => {
                burn(ctx, &mut gas_used, gas_limit, BALANCE_VALUES_GAS)?;
                state.apply_pending(ctx).map_err(fatal)?;
                // An account outside the journal can still carry a balance the
                // fields do not reflect, e.g. from a state override.
                state.sync(ctx, call.account).map_err(fatal)?;
                let fields = state.fields(ctx, call.account).map_err(fatal)?;
                finish_call::<IApeArbInfo::getBalanceValuesCall>(
                    gas_limit,
                    gas_used,
                    (fields.fixed, fields.shares, fields.debt).into(),
                )
            }
            IApeArbInfo::IApeArbInfoCalls::getYieldConfiguration(call) => {
                burn(ctx, &mut gas_used, gas_limit, BALANCE_VALUES_GAS)?;
                let fields = state.fields(ctx, call.account).map_err(fatal)?;
                finish_call::<IApeArbInfo::getYieldConfigurationCall>(
                    gas_limit,
                    gas_used,
                    fields.flags,
                )
            }
            IApeArbInfo::IApeArbInfoCalls::getDelegate(call) => {
                burn(ctx, &mut gas_used, gas_limit, BALANCE_VALUES_GAS)?;
                let fields = state.fields(ctx, call.account).map_err(fatal)?;
                finish_call::<IApeArbInfo::getDelegateCall>(gas_limit, gas_used, fields.delegate)
            }
            IApeArbInfo::IApeArbInfoCalls::configureAutomaticYield(_) => {
                state.sync(ctx, caller).map_err(fatal)?;
                state
                    .set_flags(ctx, caller, APE_YIELD_AUTOMATIC, None)
                    .map_err(fatal)?;
                state.write(ctx).map_err(fatal)?;
                finish_call::<IApeArbInfo::configureAutomaticYieldCall>(
                    gas_limit,
                    gas_used,
                    ().into(),
                )
            }
            IApeArbInfo::IApeArbInfoCalls::configureVoidYield(_) => {
                state.sync(ctx, caller).map_err(fatal)?;
                state
                    .set_flags(ctx, caller, APE_YIELD_DISABLED, None)
                    .map_err(fatal)?;
                state.write(ctx).map_err(fatal)?;
                finish_call::<IApeArbInfo::configureVoidYieldCall>(gas_limit, gas_used, ().into())
            }
            IApeArbInfo::IApeArbInfoCalls::configureDelegateYield(call) => {
                state.apply_pending(ctx).map_err(fatal)?;
                state.sync(ctx, call.account).map_err(fatal)?;
                let mut gas = CONFIGURE_DELEGATE_YIELD_GAS;
                if state.is_empty(ctx, call.account).map_err(fatal)? {
                    gas += CALL_NEW_ACCOUNT_GAS;
                }
                burn(ctx, &mut gas_used, gas_limit, gas)?;
                state
                    .set_flags(ctx, caller, APE_YIELD_DELEGATED, Some(call.account))
                    .map_err(fatal)?;
                state.write(ctx).map_err(fatal)?;
                finish_call::<IApeArbInfo::configureDelegateYieldCall>(
                    gas_limit,
                    gas_used,
                    ().into(),
                )
            }
        }
    })
}

/// The ApeChain ArbOwnerPublic methods.
pub(super) fn run_owner_public<DB: Database>(
    input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
) -> PrecompileResult {
    let gas_limit = input.gas;
    let ctx = input.context;
    dispatch::<IApeArbOwnerPublic::IApeArbOwnerPublicCalls>(
        input.data,
        gas_limit,
        |call, initial_gas| match call {
            IApeArbOwnerPublic::IApeArbOwnerPublicCalls::getSharePrice(_) => {
                let mut storage = ArbStorage::new_with_initial_gas(ctx, gas_limit, initial_gas);
                let price = storage.root(APE_SHARE_PRICE_OFFSET)?.wrapping_to::<u64>();
                finish_call::<IApeArbOwnerPublic::getSharePriceCall>(
                    gas_limit,
                    storage.gas_used,
                    price,
                )
            }
            IApeArbOwnerPublic::IApeArbOwnerPublicCalls::getShareCount(_) => {
                let mut state = YieldState::new(ctx).map_err(fatal)?;
                state.apply_pending(ctx).map_err(fatal)?;
                let mut storage = ArbStorage::new_with_initial_gas(ctx, gas_limit, initial_gas);
                let count = storage
                    .root(APE_SHARE_COUNT_OFFSET)?
                    .wrapping_add(state.share_delta());
                // Nitro returns the two's-complement word as a uint256.
                finish_call::<IApeArbOwnerPublic::getShareCountCall>(
                    gas_limit,
                    storage.gas_used,
                    count,
                )
            }
            IApeArbOwnerPublic::IApeArbOwnerPublicCalls::getApy(_) => {
                let mut storage = ArbStorage::new_with_initial_gas(ctx, gas_limit, initial_gas);
                let apy = storage.root(APE_APY_OFFSET)?.wrapping_to::<u64>();
                finish_call::<IApeArbOwnerPublic::getApyCall>(gas_limit, storage.gas_used, apy)
            }
        },
    )
}

/// The ApeChain ArbOwner setters (`arbosState.SetSharePrice` / `SetApy`).
/// Returns `false` where nitro returns an error.
pub(super) fn run_owner_write<DB: Database>(
    storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
    call: IApeArbOwner::IApeArbOwnerCalls,
) -> Result<bool, PrecompileError> {
    let time = storage.context.block().timestamp().saturating_to::<u64>();
    match call {
        IApeArbOwner::IApeArbOwnerCalls::setSharePrice(call) => {
            let current = storage.root(APE_SHARE_PRICE_OFFSET)?.wrapping_to::<u64>();
            if call.sharePrice < current || current == 0 || call.sharePrice / current > 10 {
                return Ok(false);
            }
            storage.write(&[], APE_BASE_SHARE_TIME_OFFSET, U256::from(time))?;
            storage.write(
                &[],
                APE_BASE_SHARE_PRICE_OFFSET,
                U256::from(call.sharePrice),
            )?;
            storage.write(&[], APE_SHARE_PRICE_OFFSET, U256::from(call.sharePrice))?;
        }
        IApeArbOwner::IApeArbOwnerCalls::setApy(call) => {
            if call.apy > MAX_APY {
                return Ok(false);
            }
            let base_time = storage
                .root(APE_BASE_SHARE_TIME_OFFSET)?
                .wrapping_to::<u64>();
            if time < base_time {
                return Ok(false);
            }
            let price = storage.root(APE_SHARE_PRICE_OFFSET)?;
            storage.write(&[], APE_BASE_SHARE_TIME_OFFSET, U256::from(time))?;
            storage.write(&[], APE_BASE_SHARE_PRICE_OFFSET, price)?;
            storage.write(&[], APE_APY_OFFSET, U256::from(call.apy))?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::super::util::copy_gas;
    use super::super::BASE_PRECOMPILE_GAS;
    use super::*;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::sol_types::SolCall;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::state::AccountInfo;
    use revm::{Context, MainContext};

    type Ctx = ArbitrumContext<CacheDB<EmptyDB>>;

    /// Leaves a remainder for most amounts.
    const PRICE: u64 = 1_000_000_007;

    fn context(ape_yield: bool) -> Ctx {
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_storage(
            arbos_state::ARBOS_STATE_ADDRESS,
            share_price_slot(),
            U256::from(PRICE),
        )
        .unwrap();
        let mut chain = ArbitrumExecutionContext::default();
        chain.set_ape_yield(ape_yield);
        Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv::default())
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(db)
            .with_chain(chain)
    }

    /// An account whose resolved balance and yield slots agree, as written by
    /// the writer and read through `ApeYieldStorage`.
    fn seed(ctx: &mut Ctx, address: Address, fields: ApeYieldFields) {
        let balance = fields.balance(U256::from(PRICE));
        let db = ctx.db_mut();
        db.insert_account_info(
            address,
            AccountInfo {
                balance,
                nonce: 1,
                ..Default::default()
            },
        );
        let hash = alloy::primitives::keccak256(address);
        for (i, value) in fields.to_slots().into_iter().enumerate() {
            db.insert_account_storage(APE_YIELD_ADDRESS, ape_yield_slot(hash, i), value)
                .unwrap();
        }
    }

    fn automatic(balance: u64) -> ApeYieldFields {
        let (shares, fixed) = ape_shares_and_remainder(U256::from(PRICE), U256::from(balance));
        ApeYieldFields {
            fixed,
            shares,
            ..Default::default()
        }
    }

    /// The yield slots of `address` in the journal.
    fn written(ctx: &mut Ctx, address: Address) -> ApeYieldFields {
        let mut state = YieldState::new(ctx).unwrap();
        state.fields(ctx, address).unwrap()
    }

    fn share_count(ctx: &mut Ctx) -> U256 {
        ctx.journal_mut()
            .load_account(arbos_state::ARBOS_STATE_ADDRESS)
            .unwrap();
        ctx.journal_mut()
            .sload(arbos_state::ARBOS_STATE_ADDRESS, share_count_slot())
            .unwrap()
            .data
    }

    fn input<'a>(data: &'a [u8], caller: Address, ctx: &'a mut Ctx) -> ArbPrecompileInput<'a, Ctx> {
        ArbPrecompileInput {
            data,
            gas: 1_000_000,
            caller,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 59,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: ctx,
        }
    }

    fn balance_values(ctx: &mut Ctx, account: Address) -> (U256, U256, U256, u64) {
        let data = IApeArbInfo::getBalanceValuesCall { account }.abi_encode();
        let out = run_arb_info(input(&data, Address::ZERO, ctx)).unwrap();
        assert!(!out.reverted);
        let ret = IApeArbInfo::getBalanceValuesCall::abi_decode_returns(&out.bytes).unwrap();
        (ret._0, ret._1, ret._2, out.gas_used)
    }

    fn configure(ctx: &mut Ctx, caller: Address, data: Vec<u8>) -> u64 {
        let out = run_arb_info(input(&data, caller, ctx)).unwrap();
        assert!(!out.reverted);
        out.gas_used
    }

    const A: Address = Address::repeat_byte(0xa0);
    const D: Address = Address::repeat_byte(0xd0);
    const E: Address = Address::repeat_byte(0xe0);
    const X: Address = Address::repeat_byte(0xc0);
    const OWNER: Address = Address::repeat_byte(0x0e);

    fn p(n: u64) -> U256 {
        U256::from(PRICE) * U256::from(n)
    }

    #[test]
    fn getters_read_stored_fields_with_nitro_gas() {
        let mut ctx = context(true);
        seed(&mut ctx, A, automatic(5 * PRICE + 123));
        let (fixed, shares, debt, gas) = balance_values(&mut ctx, A);
        assert_eq!(
            (fixed, shares, debt),
            (U256::from(123), U256::from(5), U256::ZERO)
        );
        assert_eq!(
            gas,
            BASE_PRECOMPILE_GAS + copy_gas(32) + BALANCE_VALUES_GAS + copy_gas(96)
        );

        let data = IApeArbInfo::getYieldConfigurationCall { account: A }.abi_encode();
        let out = run_arb_info(input(&data, Address::ZERO, &mut ctx)).unwrap();
        assert_eq!(
            IApeArbInfo::getYieldConfigurationCall::abi_decode_returns(&out.bytes).unwrap(),
            0
        );
        assert_eq!(
            out.gas_used,
            copy_gas(32) + BALANCE_VALUES_GAS + copy_gas(32)
        );

        let data = IApeArbInfo::getDelegateCall { account: E }.abi_encode();
        let out = run_arb_info(input(&data, Address::ZERO, &mut ctx)).unwrap();
        assert_eq!(
            IApeArbInfo::getDelegateCall::abi_decode_returns(&out.bytes).unwrap(),
            Address::ZERO
        );
    }

    #[test]
    fn pending_balance_change_is_replayed_on_read() {
        let mut ctx = context(true);
        seed(&mut ctx, A, automatic(5 * PRICE + 123));
        ctx.journal_mut().load_account(A).unwrap();
        ctx.journal_mut()
            .balance_incr(A, U256::from(PRICE - 100))
            .unwrap();

        let (fixed, shares, debt, _) = balance_values(&mut ctx, A);
        assert_eq!(
            (fixed, shares, debt),
            (U256::from(23), U256::from(6), U256::ZERO)
        );
        // Reads do not write.
        assert_eq!(written(&mut ctx, A), automatic(5 * PRICE + 123));
    }

    #[test]
    fn delegator_change_moves_delegate_debt() {
        let mut ctx = context(true);
        // D holds 1000 of its own and 300 delegated by X.
        seed(
            &mut ctx,
            D,
            ApeYieldFields {
                fixed: U256::from(1_300),
                debt: U256::from(300),
                ..Default::default()
            },
        );
        seed(
            &mut ctx,
            X,
            ApeYieldFields {
                flags: APE_YIELD_DELEGATED,
                fixed: U256::from(300),
                delegate: D,
                ..Default::default()
            },
        );
        ctx.journal_mut().load_account(X).unwrap();
        ctx.journal_mut().balance_incr(X, U256::from(50)).unwrap();

        let (fixed, shares, debt, _) = balance_values(&mut ctx, D);
        assert_eq!(
            (fixed, shares, debt),
            (U256::from(1_350), U256::ZERO, U256::from(350))
        );
        let (fixed, _, debt, _) = balance_values(&mut ctx, X);
        assert_eq!((fixed, debt), (U256::from(350), U256::ZERO));
    }

    #[test]
    fn spending_delegator_takes_shares_from_an_automatic_delegate() {
        use revm::context_interface::journaled_state::account::JournaledAccountTr;
        let mut ctx = context(true);
        // D: 2 shares + 7 of its own, plus 3 shares' worth delegated by X.
        seed(
            &mut ctx,
            D,
            ApeYieldFields {
                fixed: U256::from(7),
                shares: U256::from(5),
                debt: p(3),
                ..Default::default()
            },
        );
        seed(
            &mut ctx,
            X,
            ApeYieldFields {
                flags: APE_YIELD_DELEGATED,
                fixed: p(3),
                delegate: D,
                ..Default::default()
            },
        );
        // X spends two shares' worth: more than D's `fixed`, so nitro's
        // uint256 delta wraps `fixed` and the re-split removes two shares.
        assert!(ctx
            .journal_mut()
            .load_account_mut(X)
            .unwrap()
            .data
            .decr_balance(p(2)));

        let (fixed, shares, debt, _) = balance_values(&mut ctx, D);
        assert_eq!((fixed, shares, debt), (U256::from(7), U256::from(3), p(1)));
        let mut state = YieldState::new(&mut ctx).unwrap();
        state.apply_pending(&mut ctx).unwrap();
        assert_eq!(state.share_delta(), U256::ZERO.wrapping_sub(U256::from(2)));
    }

    #[test]
    fn read_replays_a_balance_set_outside_the_journal() {
        let mut ctx = context(true);
        seed(&mut ctx, A, automatic(5 * PRICE + 123));
        // A state override: the database balance changes, the yield slots do not.
        let mut info = ctx.db_mut().basic(A).unwrap().unwrap();
        info.balance = U256::from(12_345);
        ctx.db_mut().insert_account_info(A, info);

        let (fixed, shares, debt, _) = balance_values(&mut ctx, A);
        assert_eq!(
            (fixed, shares, debt),
            (U256::from(12_345), U256::ZERO, U256::ZERO)
        );
    }

    #[test]
    fn configure_delegate_yield_moves_value_into_delegate_debt() {
        let mut ctx = context(true);
        let balance = 5 * PRICE + 123;
        seed(&mut ctx, A, automatic(balance));
        seed(&mut ctx, D, automatic(1_000));
        let gas = configure(
            &mut ctx,
            A,
            IApeArbInfo::configureDelegateYieldCall { account: D }.abi_encode(),
        );
        assert_eq!(gas, copy_gas(32) + CONFIGURE_DELEGATE_YIELD_GAS);

        let a = written(&mut ctx, A);
        assert_eq!(
            a,
            ApeYieldFields {
                flags: APE_YIELD_DELEGATED,
                fixed: U256::from(balance),
                delegate: D,
                ..Default::default()
            }
        );
        // Nitro adjustDebt: D keeps its balance; its shares absorb the new fixed.
        let d = written(&mut ctx, D);
        assert_eq!(
            (d.fixed, d.shares, d.debt),
            (U256::from(1_123), U256::from(5), U256::from(balance))
        );
        assert_eq!(d.balance(U256::from(PRICE)), U256::from(1_000));
        // A's shares moved to D.
        assert_eq!(share_count(&mut ctx), U256::ZERO);
    }

    #[test]
    fn configure_delegate_yield_charges_new_account_gas_for_empty_target() {
        let mut ctx = context(true);
        seed(&mut ctx, A, automatic(100));
        seed(&mut ctx, X, automatic(7));
        let gas = configure(
            &mut ctx,
            A,
            IApeArbInfo::configureDelegateYieldCall { account: E }.abi_encode(),
        );
        assert_eq!(
            gas,
            copy_gas(32) + CONFIGURE_DELEGATE_YIELD_GAS + CALL_NEW_ACCOUNT_GAS
        );
        let e = written(&mut ctx, E);
        assert_eq!((e.fixed, e.debt), (U256::from(100), U256::from(100)));
        // E now holds yield fields, so it is no longer empty.
        let gas = configure(
            &mut ctx,
            X,
            IApeArbInfo::configureDelegateYieldCall { account: E }.abi_encode(),
        );
        assert_eq!(gas, copy_gas(32) + CONFIGURE_DELEGATE_YIELD_GAS);
    }

    #[test]
    fn self_delegation_and_unchanged_mode_are_noops() {
        let mut ctx = context(true);
        seed(&mut ctx, A, automatic(100));
        configure(
            &mut ctx,
            A,
            IApeArbInfo::configureDelegateYieldCall { account: A }.abi_encode(),
        );
        assert_eq!(
            configure(
                &mut ctx,
                A,
                IApeArbInfo::configureAutomaticYieldCall {}.abi_encode()
            ),
            0
        );
        assert_eq!(written(&mut ctx, A), automatic(100));
    }

    #[test]
    fn void_then_automatic_keeps_balance_and_share_count() {
        let mut ctx = context(true);
        ctx.db_mut()
            .insert_account_storage(
                arbos_state::ARBOS_STATE_ADDRESS,
                share_count_slot(),
                U256::from(5),
            )
            .unwrap();
        seed(&mut ctx, A, automatic(5 * PRICE + 123));
        configure(
            &mut ctx,
            A,
            IApeArbInfo::configureVoidYieldCall {}.abi_encode(),
        );
        assert_eq!(
            written(&mut ctx, A),
            ApeYieldFields {
                flags: APE_YIELD_DISABLED,
                fixed: p(5) + U256::from(123),
                ..Default::default()
            }
        );
        assert_eq!(share_count(&mut ctx), U256::ZERO);
        configure(
            &mut ctx,
            A,
            IApeArbInfo::configureAutomaticYieldCall {}.abi_encode(),
        );
        assert_eq!(written(&mut ctx, A), automatic(5 * PRICE + 123));
        assert_eq!(share_count(&mut ctx), U256::from(5));
    }

    #[test]
    fn configure_after_a_balance_change_uses_the_new_balance() {
        let mut ctx = context(true);
        ctx.db_mut()
            .insert_account_storage(
                arbos_state::ARBOS_STATE_ADDRESS,
                share_count_slot(),
                U256::from(5),
            )
            .unwrap();
        seed(&mut ctx, A, automatic(5 * PRICE + 123));
        ctx.journal_mut().load_account(A).unwrap();
        ctx.journal_mut()
            .balance_incr(A, U256::from(PRICE))
            .unwrap();
        configure(
            &mut ctx,
            A,
            IApeArbInfo::configureVoidYieldCall {}.abi_encode(),
        );
        assert_eq!(written(&mut ctx, A).fixed, p(6) + U256::from(123));
        // Nitro: SetBalance takes shareCount from 5 to 6, SetFlags to 0.
        assert_eq!(share_count(&mut ctx), U256::ZERO);
    }

    #[test]
    fn owner_public_getters_include_pending_shares() {
        let mut ctx = context(true);
        ctx.db_mut()
            .insert_account_storage(
                arbos_state::ARBOS_STATE_ADDRESS,
                share_count_slot(),
                U256::from(5),
            )
            .unwrap();
        seed(&mut ctx, A, automatic(5 * PRICE));
        ctx.journal_mut().load_account(A).unwrap();
        ctx.journal_mut()
            .balance_incr(A, U256::from(PRICE))
            .unwrap();
        let call = |ctx: &mut Ctx, data: Vec<u8>| {
            let out = run_owner_public(input(&data, Address::ZERO, ctx)).unwrap();
            assert_eq!(out.gas_used, 800 + copy_gas(32));
            U256::from_be_slice(&out.bytes)
        };
        assert_eq!(
            call(
                &mut ctx,
                IApeArbOwnerPublic::getShareCountCall {}.abi_encode()
            ),
            U256::from(6)
        );
        assert_eq!(
            call(
                &mut ctx,
                IApeArbOwnerPublic::getSharePriceCall {}.abi_encode()
            ),
            U256::from(PRICE)
        );
        assert_eq!(
            call(&mut ctx, IApeArbOwnerPublic::getApyCall {}.abi_encode()),
            U256::ZERO
        );
    }

    #[test]
    fn ape_methods_need_ape_yield() {
        use super::super::arb_info::ArbInfo;
        use super::super::registry::ArbitrumPrecompile;
        let data = IApeArbInfo::getBalanceValuesCall { account: A }.abi_encode();
        let mut ctx = context(false);
        assert!(
            ArbInfo::run(input(&data, Address::ZERO, &mut ctx))
                .unwrap()
                .reverted
        );
        assert_eq!(ArbitrumPrecompile::ArbInfo.purity(&data, false), None);
        assert_eq!(
            ArbitrumPrecompile::ArbInfo.purity(&data, true),
            Some(ArbitrumPrecompilePurity::View)
        );
        let write = IApeArbInfo::configureVoidYieldCall {}.abi_encode();
        assert_eq!(
            ArbitrumPrecompile::ArbInfo.purity(&write, true),
            Some(ArbitrumPrecompilePurity::Write)
        );
        let price = IApeArbOwnerPublic::getSharePriceCall {}.abi_encode();
        assert_eq!(
            ArbitrumPrecompile::ArbOwnerPublic.purity(&price, false),
            None
        );

        let mut ctx = context(true);
        seed(&mut ctx, A, automatic(5));
        assert!(
            !ArbInfo::run(input(&data, Address::ZERO, &mut ctx))
                .unwrap()
                .reverted
        );
    }

    #[test]
    fn owner_setters_follow_arbos_rules() {
        use super::super::owner::ArbOwner;
        let mut ctx = context(true);
        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut ctx, u64::MAX, 0);
            let key = storage.chain_owner_key();
            storage.address_set_add(&key, OWNER).unwrap();
        }
        let send = |ctx: &mut Ctx, caller: Address, data: Vec<u8>| {
            ArbOwner::run(input(&data, caller, ctx)).unwrap().reverted
        };
        let root = |ctx: &mut Ctx, offset: u64| {
            ctx.journal_mut()
                .sload(
                    arbos_state::ARBOS_STATE_ADDRESS,
                    arbos_state::slot_at(&[], offset),
                )
                .unwrap()
                .data
        };
        let set_price =
            |price: u64| IApeArbOwner::setSharePriceCall { sharePrice: price }.abi_encode();
        assert!(send(&mut ctx, A, set_price(2 * PRICE)), "not an owner");
        assert!(!send(&mut ctx, OWNER, set_price(2 * PRICE)));
        assert_eq!(root(&mut ctx, APE_SHARE_PRICE_OFFSET), p(2));
        assert_eq!(root(&mut ctx, APE_BASE_SHARE_PRICE_OFFSET), p(2));
        assert!(
            send(&mut ctx, OWNER, set_price(PRICE)),
            "cannot lower the price"
        );
        assert!(
            send(&mut ctx, OWNER, set_price(23 * PRICE)),
            "more than 10x"
        );
        assert!(send(
            &mut ctx,
            OWNER,
            IApeArbOwner::setApyCall { apy: MAX_APY + 1 }.abi_encode()
        ));
        assert!(!send(
            &mut ctx,
            OWNER,
            IApeArbOwner::setApyCall { apy: 5_000_000_000 }.abi_encode()
        ));
        assert_eq!(root(&mut ctx, APE_APY_OFFSET), U256::from(5_000_000_000u64));
    }

    #[test]
    fn create_hook_disables_yield_after_the_value_transfer() {
        let mut ctx = context(true);
        ctx.journal_mut().load_account(E).unwrap();
        ctx.journal_mut()
            .balance_incr(E, U256::from(1_000))
            .unwrap();
        on_create(&mut ctx, E).unwrap();
        assert_eq!(
            written(&mut ctx, E),
            ApeYieldFields {
                flags: APE_YIELD_DISABLED,
                fixed: U256::from(1_000),
                ..Default::default()
            }
        );
        assert_eq!(share_count(&mut ctx), U256::ZERO);

        // A pre-funded address keeps its value; its shares leave shareCount.
        seed(&mut ctx, X, automatic(5 * PRICE + 1));
        ctx.journal_mut().load_account(X).unwrap();
        ctx.journal_mut().balance_incr(X, U256::from(10)).unwrap();
        on_create(&mut ctx, X).unwrap();
        assert_eq!(written(&mut ctx, X).fixed, p(5) + U256::from(11));
        assert_eq!(
            share_count(&mut ctx),
            U256::ZERO.wrapping_sub(U256::from(5))
        );
    }

    #[test]
    fn selfdestruct_hook_unwinds_the_delegation() {
        let mut ctx = context(true);
        seed(
            &mut ctx,
            D,
            ApeYieldFields {
                fixed: U256::from(1_300),
                debt: U256::from(300),
                ..Default::default()
            },
        );
        seed(
            &mut ctx,
            X,
            ApeYieldFields {
                flags: APE_YIELD_DELEGATED,
                fixed: U256::from(300),
                delegate: D,
                ..Default::default()
            },
        );
        ctx.journal_mut().load_account(X).unwrap();
        ctx.journal_mut().load_account(E).unwrap();
        ctx.journal_mut().transfer(X, E, U256::from(300)).unwrap();
        on_selfdestruct(&mut ctx, X).unwrap();
        assert_eq!(
            written(&mut ctx, X),
            ApeYieldFields {
                flags: APE_YIELD_DISABLED,
                ..Default::default()
            }
        );
        let d = written(&mut ctx, D);
        assert_eq!(
            (d.fixed, d.shares, d.debt),
            (U256::from(1_000), U256::ZERO, U256::ZERO)
        );
    }
}
