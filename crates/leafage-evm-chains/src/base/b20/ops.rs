//! B20 token operations.
//!
//! Transcribed from Base reth's capability traits
//! (`base/crates/common/precompiles/src/common/ops/*.rs`). The *order* of the checks in each
//! function is load-bearing twice over: it decides which error a failing call reports, and —
//! because every storage touch is metered — it decides the gas a succeeding call costs.
//! Reordering a guard here silently changes both. Base pins the ordering with its
//! `*_check_order` / `*_guard_ordering` tests; the equivalents live in `tests` below.

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolEvent;

use super::abi::{IB20, IB20Asset};
use super::error::{B20Error, Result};
use super::ids;
use super::layout::{checked_add, checked_sub, checked_mul, B20Store, PolicySlot, WAD};
use super::policy;
use super::port::B20Port;

/// Maximum total supply for a B20 token: `2^128 - 1`.
pub const B20_MAX_SUPPLY_CAP: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

// --- Guards ---

/// Reverts unless `feature` is currently unpaused.
pub fn ensure_not_paused<P: B20Port>(
    store: &mut B20Store<'_, P>,
    feature: IB20::PausableFeature,
) -> Result<()> {
    if (store.paused()? & ids::pause_mask(feature)).is_zero() {
        Ok(())
    } else {
        Err(B20Error::revert(IB20::ContractPaused { feature }))
    }
}

/// Reverts unless `caller` holds `role`.
pub fn ensure_role<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    role: B256,
) -> Result<()> {
    if store.has_role(role, caller)? {
        Ok(())
    } else {
        Err(B20Error::revert(IB20::AccessControlUnauthorizedAccount {
            account: caller,
            neededRole: role,
        }))
    }
}

/// Reverts unless `account` is authorized by the policy configured for `slot_kind`.
///
/// The configured ID is read from the token, then delegated to the policy registry —
/// including for the built-in IDs, which the registry resolves without touching storage.
pub fn ensure_policy<P: B20Port>(
    store: &mut B20Store<'_, P>,
    slot_kind: PolicySlot,
    scope: B256,
    account: Address,
) -> Result<()> {
    let policy_id = store.policy_id(slot_kind)?;
    if policy::is_authorized(store.port(), policy_id, account)? {
        Ok(())
    } else {
        Err(B20Error::revert(IB20::PolicyForbids { policyScope: scope, policyId: policy_id }))
    }
}

/// Reverts unless `account` is *denied* by the transfer-sender policy.
pub fn ensure_blocked<P: B20Port>(store: &mut B20Store<'_, P>, account: Address) -> Result<()> {
    let policy_id = store.policy_id(PolicySlot::TransferSender)?;
    if policy::is_authorized(store.port(), policy_id, account)? {
        Err(B20Error::revert(IB20::AccountNotBlocked { account }))
    } else {
        Ok(())
    }
}

fn emit<P: B20Port>(store: &mut B20Store<'_, P>, log: alloy::primitives::LogData) -> Result<()> {
    let address = store.address();
    store.port().emit_event(address, log)
}

// --- Transfers ---

/// Moves `amount` from `from` to `to`, emitting `Transfer`.
pub fn transfer<P: B20Port>(
    store: &mut B20Store<'_, P>,
    from: Address,
    to: Address,
    amount: U256,
    privileged: bool,
) -> Result<()> {
    ensure_not_paused(store, IB20::PausableFeature::TRANSFER)?;
    transfer_inner(store, from, to, amount, privileged)
}

/// Transfer body, without the pause check (`transfer_from` runs its own first).
fn transfer_inner<P: B20Port>(
    store: &mut B20Store<'_, P>,
    from: Address,
    to: Address,
    amount: U256,
    privileged: bool,
) -> Result<()> {
    if to == Address::ZERO {
        return Err(B20Error::revert(IB20::InvalidReceiver { receiver: to }));
    }
    if from == Address::ZERO {
        return Err(B20Error::revert(IB20::InvalidSender { sender: from }));
    }
    if !privileged {
        ensure_policy(store, PolicySlot::TransferSender, ids::TRANSFER_SENDER_POLICY, from)?;
        ensure_policy(store, PolicySlot::TransferReceiver, ids::TRANSFER_RECEIVER_POLICY, to)?;
    }

    let from_balance = store.balance_of(from)?;
    if from_balance < amount {
        return Err(B20Error::revert(IB20::InsufficientBalance {
            sender: from,
            balance: from_balance,
            needed: amount,
        }));
    }
    store.set_balance(from, checked_sub(from_balance, amount)?)?;

    // Re-read after the debit: when `from == to` the second read must observe the debited
    // value, otherwise a self-transfer would mint `amount` out of thin air.
    let to_balance = store.balance_of(to)?;
    store.set_balance(to, checked_add(to_balance, amount)?)?;

    emit(store, IB20::Transfer { from, to, amount }.encode_log_data())
}

/// Spends `spender`'s allowance to move `amount` from `from` to `to`.
pub fn transfer_from<P: B20Port>(
    store: &mut B20Store<'_, P>,
    spender: Address,
    from: Address,
    to: Address,
    amount: U256,
    privileged: bool,
) -> Result<()> {
    ensure_not_paused(store, IB20::PausableFeature::TRANSFER)?;
    if to == Address::ZERO {
        return Err(B20Error::revert(IB20::InvalidReceiver { receiver: to }));
    }
    if from == Address::ZERO {
        return Err(B20Error::revert(IB20::InvalidSender { sender: from }));
    }

    let allowance = store.allowance(from, spender)?;
    let is_infinite = allowance == U256::MAX;
    if !is_infinite && allowance < amount {
        return Err(B20Error::revert(IB20::InsufficientAllowance {
            spender,
            allowance,
            needed: amount,
        }));
    }
    // Runs even for an infinite allowance: an unlimited approval does not exempt the
    // executor from the policy.
    if !privileged && spender != from {
        ensure_policy(store, PolicySlot::TransferExecutor, ids::TRANSFER_EXECUTOR_POLICY, spender)?;
    }

    transfer_inner(store, from, to, amount, privileged)?;

    if is_infinite {
        return Ok(());
    }
    store.set_allowance(from, spender, allowance - amount)
}

/// Sets `spender`'s allowance from `owner`, emitting `Approval`. No pause check.
pub fn approve<P: B20Port>(
    store: &mut B20Store<'_, P>,
    owner: Address,
    spender: Address,
    amount: U256,
) -> Result<()> {
    if owner == Address::ZERO {
        return Err(B20Error::revert(IB20::InvalidApprover { approver: owner }));
    }
    if spender == Address::ZERO {
        return Err(B20Error::revert(IB20::InvalidSpender { spender }));
    }
    store.set_allowance(owner, spender, amount)?;
    emit(store, IB20::Approval { owner, spender, amount }.encode_log_data())
}

/// [`transfer`] followed by a `Memo` event.
pub fn transfer_with_memo<P: B20Port>(
    store: &mut B20Store<'_, P>,
    from: Address,
    to: Address,
    amount: U256,
    memo: B256,
    privileged: bool,
) -> Result<()> {
    transfer(store, from, to, amount, privileged)?;
    emit(store, IB20::Memo { caller: from, memo }.encode_log_data())
}

/// [`transfer_from`] followed by a `Memo` event.
pub fn transfer_from_with_memo<P: B20Port>(
    store: &mut B20Store<'_, P>,
    spender: Address,
    from: Address,
    to: Address,
    amount: U256,
    memo: B256,
    privileged: bool,
) -> Result<()> {
    transfer_from(store, spender, from, to, amount, privileged)?;
    emit(store, IB20::Memo { caller: spender, memo }.encode_log_data())
}

// --- Mint ---

/// Creates `amount` tokens at `to`, enforcing the supply cap.
pub fn mint<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    to: Address,
    amount: U256,
    privileged: bool,
) -> Result<()> {
    ensure_not_paused(store, IB20::PausableFeature::MINT)?;
    if !privileged {
        ensure_role(store, caller, ids::MINT_ROLE)?;
    }
    if to == Address::ZERO {
        return Err(B20Error::revert(IB20::InvalidReceiver { receiver: to }));
    }
    // Unlike transfer, the mint-receiver policy is enforced even when privileged.
    ensure_policy(store, PolicySlot::MintReceiver, ids::MINT_RECEIVER_POLICY, to)?;

    let supply = store.total_supply()?;
    let cap = store.supply_cap()?;
    let new_supply = checked_add(supply, amount)?;
    if new_supply > cap {
        return Err(B20Error::revert(IB20::SupplyCapExceeded { cap, attempted: new_supply }));
    }
    store.set_total_supply(new_supply)?;
    let to_balance = store.balance_of(to)?;
    store.set_balance(to, checked_add(to_balance, amount)?)?;
    emit(store, IB20::Transfer { from: Address::ZERO, to, amount }.encode_log_data())
}

/// [`mint`] followed by a `Memo` event.
pub fn mint_with_memo<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    to: Address,
    amount: U256,
    memo: B256,
    privileged: bool,
) -> Result<()> {
    mint(store, caller, to, amount, privileged)?;
    emit(store, IB20::Memo { caller, memo }.encode_log_data())
}

// --- Burn ---

/// Destroys `amount` tokens held by `from`.
pub fn burn<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    from: Address,
    amount: U256,
    privileged: bool,
) -> Result<()> {
    ensure_not_paused(store, IB20::PausableFeature::BURN)?;
    if !privileged {
        ensure_role(store, caller, ids::BURN_ROLE)?;
    }
    burn_inner(store, from, amount)
}

fn burn_inner<P: B20Port>(
    store: &mut B20Store<'_, P>,
    from: Address,
    amount: U256,
) -> Result<()> {
    let balance = store.balance_of(from)?;
    if balance < amount {
        return Err(B20Error::revert(IB20::InsufficientBalance {
            sender: from,
            balance,
            needed: amount,
        }));
    }
    store.set_balance(from, balance - amount)?;
    let supply = store.total_supply()?;
    store.set_total_supply(checked_sub(supply, amount)?)?;
    emit(store, IB20::Transfer { from, to: Address::ZERO, amount }.encode_log_data())
}

/// [`burn`] followed by a `Memo` event.
pub fn burn_with_memo<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    from: Address,
    amount: U256,
    memo: B256,
    privileged: bool,
) -> Result<()> {
    burn(store, caller, from, amount, privileged)?;
    emit(store, IB20::Memo { caller, memo }.encode_log_data())
}

/// Destroys `amount` from a policy-blocked account.
pub fn burn_blocked<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    from: Address,
    amount: U256,
    privileged: bool,
) -> Result<()> {
    ensure_not_paused(store, IB20::PausableFeature::BURN)?;
    if !privileged {
        ensure_role(store, caller, ids::BURN_BLOCKED_ROLE)?;
    }
    ensure_blocked(store, from)?;
    burn_inner(store, from, amount)?;
    emit(store, IB20::BurnedBlocked { caller, from, amount }.encode_log_data())
}

// --- Pause ---

/// Whether `feature` is currently paused.
pub fn is_paused<P: B20Port>(
    store: &mut B20Store<'_, P>,
    feature: IB20::PausableFeature,
) -> Result<bool> {
    ensure_valid_feature(feature)?;
    Ok(!(store.paused()? & ids::pause_mask(feature)).is_zero())
}

/// All currently paused features, in enum order.
pub fn paused_features<P: B20Port>(
    store: &mut B20Store<'_, P>,
) -> Result<Vec<IB20::PausableFeature>> {
    let paused = store.paused()?;
    Ok(ids::PAUSABLE_FEATURES
        .into_iter()
        .filter(|f| !(paused & ids::pause_mask(*f)).is_zero())
        .collect())
}

fn ensure_valid_feature(feature: IB20::PausableFeature) -> Result<()> {
    match feature {
        IB20::PausableFeature::__Invalid => Err(B20Error::empty_revert()),
        _ => Ok(()),
    }
}

/// ORs `features` into the paused bitmask.
pub fn pause<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    features: Vec<IB20::PausableFeature>,
    privileged: bool,
) -> Result<()> {
    for feature in &features {
        ensure_valid_feature(*feature)?;
    }
    if !privileged {
        ensure_role(store, caller, ids::PAUSE_ROLE)?;
    }
    if features.is_empty() {
        return Err(B20Error::revert(IB20::EmptyFeatureSet {}));
    }
    let mut next = store.paused()?;
    for feature in &features {
        next |= ids::pause_mask(*feature);
    }
    store.set_paused(next)?;
    emit(store, IB20::Paused { updater: caller, features }.encode_log_data())
}

/// Clears `features` from the paused bitmask.
pub fn unpause<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    features: Vec<IB20::PausableFeature>,
    privileged: bool,
) -> Result<()> {
    for feature in &features {
        ensure_valid_feature(*feature)?;
    }
    if !privileged {
        ensure_role(store, caller, ids::UNPAUSE_ROLE)?;
    }
    if features.is_empty() {
        return Err(B20Error::revert(IB20::EmptyFeatureSet {}));
    }
    let mut next = store.paused()?;
    for feature in &features {
        next &= !ids::pause_mask(*feature);
    }
    store.set_paused(next)?;
    emit(store, IB20::Unpaused { updater: caller, features }.encode_log_data())
}

// --- Configuration ---

/// Updates the supply cap. Requires `DEFAULT_ADMIN_ROLE`.
pub fn update_supply_cap<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    new_cap: U256,
    privileged: bool,
) -> Result<()> {
    if !privileged {
        ensure_role(store, caller, ids::DEFAULT_ADMIN_ROLE)?;
    }
    let supply = store.total_supply()?;
    if new_cap < supply || new_cap > B20_MAX_SUPPLY_CAP {
        return Err(B20Error::revert(IB20::InvalidSupplyCap {
            currentSupply: supply,
            proposedCap: new_cap,
        }));
    }
    let old = store.supply_cap()?;
    store.set_supply_cap(new_cap)?;
    emit(
        store,
        IB20::SupplyCapUpdated { updater: caller, oldSupplyCap: old, newSupplyCap: new_cap }
            .encode_log_data(),
    )
}

/// Updates the token name, invalidating outstanding permit signatures.
pub fn update_name<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    name: String,
    privileged: bool,
) -> Result<()> {
    if !privileged {
        ensure_role(store, caller, ids::METADATA_ROLE)?;
    }
    store.set_name(&name)?;
    emit(store, IB20::NameUpdated { updater: caller, newName: name }.encode_log_data())?;
    emit(store, IB20::EIP712DomainChanged {}.encode_log_data())
}

/// Updates the token symbol.
pub fn update_symbol<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    symbol: String,
    privileged: bool,
) -> Result<()> {
    if !privileged {
        ensure_role(store, caller, ids::METADATA_ROLE)?;
    }
    store.set_symbol(&symbol)?;
    emit(store, IB20::SymbolUpdated { updater: caller, newSymbol: symbol }.encode_log_data())
}

/// Updates the ERC-7572 contract URI.
pub fn update_contract_uri<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    uri: String,
    privileged: bool,
) -> Result<()> {
    if !privileged {
        ensure_role(store, caller, ids::METADATA_ROLE)?;
    }
    store.set_contract_uri(&uri)?;
    emit(store, IB20::ContractURIUpdated {}.encode_log_data())
}

// --- Policy configuration ---

/// Reads the policy ID for `scope`, rejecting unknown scopes.
pub fn policy_id<P: B20Port>(store: &mut B20Store<'_, P>, scope: B256) -> Result<u64> {
    let slot_kind = ids::require_policy_slot(scope)?;
    store.policy_id(slot_kind)
}

/// Points `scope` at `new_policy_id`. Requires `DEFAULT_ADMIN_ROLE`.
pub fn update_policy<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    scope: B256,
    new_policy_id: u64,
    privileged: bool,
) -> Result<()> {
    if !privileged {
        ensure_role(store, caller, ids::DEFAULT_ADMIN_ROLE)?;
    }
    let slot_kind = ids::require_policy_slot(scope)?;
    if !policy::policy_exists(store.port(), new_policy_id)? {
        return Err(B20Error::revert(IB20::PolicyNotFound { policyId: new_policy_id }));
    }
    let old = store.policy_id(slot_kind)?;
    store.set_policy_id(slot_kind, new_policy_id)?;
    emit(
        store,
        IB20::PolicyUpdated { policyScope: scope, oldPolicyId: old, newPolicyId: new_policy_id }
            .encode_log_data(),
    )
}

// --- Roles ---

/// Member count for `role`. Only the default-admin role is counted; every other role
/// answers zero without touching storage, exactly as Base's generated accessor does.
fn role_member_count<P: B20Port>(store: &mut B20Store<'_, P>, role: B256) -> Result<U256> {
    if role == ids::DEFAULT_ADMIN_ROLE {
        store.admin_count()
    } else {
        Ok(U256::ZERO)
    }
}

fn set_role_member_count<P: B20Port>(
    store: &mut B20Store<'_, P>,
    role: B256,
    count: U256,
) -> Result<()> {
    if role == ids::DEFAULT_ADMIN_ROLE {
        store.set_admin_count(count)
    } else {
        Ok(())
    }
}

/// Reverts when no default admin remains, so role mutation cannot resurrect one.
fn ensure_role_admin_mutations_available<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
) -> Result<()> {
    if role_member_count(store, ids::DEFAULT_ADMIN_ROLE)?.is_zero() {
        return Err(B20Error::revert(IB20::AccessControlUnauthorizedAccount {
            account: caller,
            neededRole: ids::DEFAULT_ADMIN_ROLE,
        }));
    }
    Ok(())
}

fn grant_role_unchecked<P: B20Port>(
    store: &mut B20Store<'_, P>,
    role: B256,
    account: Address,
    sender: Address,
) -> Result<()> {
    if store.has_role(role, account)? {
        return Ok(());
    }
    store.set_role(role, account, true)?;
    if role == ids::DEFAULT_ADMIN_ROLE {
        let current = role_member_count(store, role)?;
        set_role_member_count(store, role, checked_add(current, U256::ONE)?)?;
    }
    emit(store, IB20::RoleGranted { role, account, sender }.encode_log_data())
}

fn revoke_role_unchecked<P: B20Port>(
    store: &mut B20Store<'_, P>,
    role: B256,
    account: Address,
    sender: Address,
) -> Result<()> {
    if !store.has_role(role, account)? {
        return Ok(());
    }
    store.set_role(role, account, false)?;
    if role == ids::DEFAULT_ADMIN_ROLE {
        let current = role_member_count(store, role)?;
        set_role_member_count(store, role, checked_sub(current, U256::ONE)?)?;
    }
    emit(store, IB20::RoleRevoked { role, account, sender }.encode_log_data())
}

/// Grants `role` to `account`.
pub fn grant_role<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    role: B256,
    account: Address,
    privileged: bool,
) -> Result<()> {
    // The admin-resurrection guard applies to DEFAULT_ADMIN_ROLE grants even when
    // privileged: `renounceLastAdmin` is a permanent terminal state.
    if role == ids::DEFAULT_ADMIN_ROLE || !privileged {
        ensure_role_admin_mutations_available(store, caller)?;
    }
    if !privileged {
        let admin = store.role_admin(role)?;
        ensure_role(store, caller, admin)?;
    }
    grant_role_unchecked(store, role, account, caller)
}

/// Revokes `role` from `account`.
pub fn revoke_role<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    role: B256,
    account: Address,
    privileged: bool,
) -> Result<()> {
    if !privileged {
        ensure_role_admin_mutations_available(store, caller)?;
        let admin = store.role_admin(role)?;
        ensure_role(store, caller, admin)?;
    }
    if role == ids::DEFAULT_ADMIN_ROLE
        && store.has_role(role, account)?
        && role_member_count(store, role)? == U256::ONE
    {
        return Err(B20Error::revert(IB20::LastAdminCannotRenounce {}));
    }
    revoke_role_unchecked(store, role, account, caller)
}

/// Renounces `role` for `caller`.
pub fn renounce_role<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    role: B256,
    confirmation: Address,
) -> Result<()> {
    if confirmation != caller {
        return Err(B20Error::revert(IB20::AccessControlBadConfirmation {}));
    }
    if role == ids::DEFAULT_ADMIN_ROLE
        && store.has_role(role, caller)?
        && role_member_count(store, role)? == U256::ONE
    {
        return Err(B20Error::revert(IB20::LastAdminCannotRenounce {}));
    }
    revoke_role_unchecked(store, role, caller, caller)
}

/// Permanently removes the final default admin.
pub fn renounce_last_admin<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
) -> Result<()> {
    ensure_role(store, caller, ids::DEFAULT_ADMIN_ROLE)?;
    if role_member_count(store, ids::DEFAULT_ADMIN_ROLE)? != U256::ONE {
        return Err(B20Error::revert(IB20::NotSoleAdmin {}));
    }
    revoke_role_unchecked(store, ids::DEFAULT_ADMIN_ROLE, caller, caller)?;
    emit(store, IB20::LastAdminRenounced { previousAdmin: caller }.encode_log_data())
}

/// Sets the admin role for `role`.
pub fn set_role_admin<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    role: B256,
    new_admin_role: B256,
    privileged: bool,
) -> Result<()> {
    let previous = store.role_admin(role)?;
    if !privileged {
        ensure_role_admin_mutations_available(store, caller)?;
        ensure_role(store, caller, previous)?;
    }
    store.set_role_admin(role, new_admin_role)?;
    emit(
        store,
        IB20::RoleAdminChanged {
            role,
            previousAdminRole: previous,
            newAdminRole: new_admin_role,
        }
        .encode_log_data(),
    )
}

// --- Asset multiplier ---

/// `rawBalance * multiplier / WAD`.
pub fn to_scaled_balance<P: B20Port>(
    store: &mut B20Store<'_, P>,
    balance: U256,
) -> Result<U256> {
    let multiplier = store.multiplier()?;
    Ok(checked_mul(balance, multiplier)? / WAD)
}

/// `scaledBalance * WAD / multiplier`.
pub fn to_raw_balance<P: B20Port>(store: &mut B20Store<'_, P>, balance: U256) -> Result<U256> {
    let multiplier = store.multiplier()?;
    Ok(checked_mul(balance, WAD)? / multiplier)
}

/// `toScaledBalance(balanceOf(account))`.
pub fn scaled_balance_of<P: B20Port>(
    store: &mut B20Store<'_, P>,
    account: Address,
) -> Result<U256> {
    let balance = store.balance_of(account)?;
    to_scaled_balance(store, balance)
}

/// Sets a new multiplier. Requires `OPERATOR_ROLE`.
pub fn update_multiplier<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    new_multiplier: U256,
    privileged: bool,
) -> Result<()> {
    if !privileged {
        ensure_role(store, caller, ids::OPERATOR_ROLE)?;
    }
    if new_multiplier.is_zero() {
        return Err(B20Error::revert(IB20Asset::InvalidMultiplier {}));
    }
    store.set_multiplier(new_multiplier)?;
    emit(store, IB20Asset::MultiplierUpdated { multiplier: new_multiplier }.encode_log_data())
}

/// Sets, updates, or removes an extra-metadata entry. Requires `METADATA_ROLE`.
pub fn update_extra_metadata<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    key: String,
    value: String,
    privileged: bool,
) -> Result<()> {
    if !privileged {
        ensure_role(store, caller, ids::METADATA_ROLE)?;
    }
    if key.is_empty() {
        return Err(B20Error::revert(IB20Asset::InvalidMetadataKey {}));
    }
    store.set_extra_metadata(&key, &value)?;
    emit(store, IB20Asset::ExtraMetadataUpdated { key, value }.encode_log_data())
}

/// Mints to many recipients. All-or-nothing.
///
/// The pause and role guards here are the *sole* authorization for the batch: the inner
/// mints run privileged to avoid re-checking per recipient. Removing either guard would
/// open a fully unauthenticated mint path.
pub fn batch_mint<P: B20Port>(
    store: &mut B20Store<'_, P>,
    caller: Address,
    recipients: Vec<Address>,
    amounts: Vec<U256>,
    privileged: bool,
) -> Result<()> {
    ensure_not_paused(store, IB20::PausableFeature::MINT)?;
    if !privileged {
        ensure_role(store, caller, ids::MINT_ROLE)?;
    }
    if recipients.len() != amounts.len() {
        return Err(B20Error::revert(IB20Asset::LengthMismatch {
            leftLen: U256::from(recipients.len()),
            rightLen: U256::from(amounts.len()),
        }));
    }
    if recipients.is_empty() {
        return Err(B20Error::revert(IB20Asset::EmptyBatch {}));
    }
    for (recipient, amount) in recipients.into_iter().zip(amounts) {
        mint(store, caller, recipient, amount, true)?;
    }
    Ok(())
}
