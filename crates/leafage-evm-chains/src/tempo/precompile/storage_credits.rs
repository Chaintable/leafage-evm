//! TIP-1060 storage credits precompile (T7+).

use std::collections::BTreeMap;

use alloy::primitives::{Address, U256};
use alloy::sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileResult};
use revm::{
    context_interface::cfg::GasParams,
    interpreter::{Gas, Host, SStoreResult, StateLoad},
};
use scoped_tls::scoped_thread_local;

use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx};
use super::storage_types::{Handler, LayoutCtx, StorableType};
use super::{
    dispatch_call, input_cost, mutate_void, unknown_selector, view, Precompile,
    STORAGE_CREDITS_ADDRESS,
};

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IStorageCredits {
        enum Mode {
            Refund,
            Preserve,
            Direct
        }

        error InvalidMode();

        function balanceOf(address account) external view returns (uint64);
        function modeOf(address account) external view returns (Mode);
        function budgetOf(address account) external view returns (uint64);

        function setMode(Mode newMode) external;
        function setBudget(uint64 credits) external;
    }
}

/// Transaction-local policy for storage creations.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CreditMode {
    #[default]
    Refund,
    Preserve,
    Direct,
}

impl TryFrom<u8> for CreditMode {
    type Error = TempoPrecompileError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Refund),
            1 => Ok(Self::Preserve),
            2 => Ok(Self::Direct),
            _ => Err(TempoPrecompileError::Revert(
                IStorageCredits::InvalidMode {}.abi_encode().into(),
            )),
        }
    }
}

impl TryFrom<IStorageCredits::Mode> for CreditMode {
    type Error = TempoPrecompileError;

    fn try_from(mode: IStorageCredits::Mode) -> Result<Self> {
        match mode {
            IStorageCredits::Mode::Refund => Ok(Self::Refund),
            IStorageCredits::Mode::Preserve => Ok(Self::Preserve),
            IStorageCredits::Mode::Direct => Ok(Self::Direct),
            IStorageCredits::Mode::__Invalid => Err(TempoPrecompileError::Revert(
                IStorageCredits::InvalidMode {}.abi_encode().into(),
            )),
        }
    }
}

impl From<CreditMode> for IStorageCredits::Mode {
    fn from(mode: CreditMode) -> Self {
        match mode {
            CreditMode::Refund => Self::Refund,
            CreditMode::Preserve => Self::Preserve,
            CreditMode::Direct => Self::Direct,
        }
    }
}

/// Packed transaction-local state stored at the same slot as the persistent balance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransientState {
    pub mode: CreditMode,
    pub budget: u64,
    pub pending_refunds: u64,
}

/// The creditable part of one T7 storage creation.
pub(crate) const STORAGE_CREDIT_VALUE: u64 = 245_000;

/// Transaction fee slots whose clear cannot mint a backed storage credit.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NonCreditableSlots {
    fee_token: Address,
    fee_balance_slot: U256,
    keychain_limit_slot: Option<U256>,
}

impl NonCreditableSlots {
    pub(crate) fn new(
        fee_payer: Address,
        fee_token: Address,
        keychain_fee_key: Option<Address>,
    ) -> Self {
        use super::account_keychain::AccountKeychain;
        use super::storage_types::StorageKey;

        let fee_balance_slot = fee_payer.mapping_slot(U256::from(9));
        let keychain_limit_slot = keychain_fee_key.map(|key_id| {
            let keychain = AccountKeychain::new();
            let limit_key = AccountKeychain::spending_limit_key(fee_payer, key_id);
            keychain.spending_limits[limit_key][fee_token]
                .remaining
                .slot()
        });
        Self {
            fee_token,
            fee_balance_slot,
            keychain_limit_slot,
        }
    }

    fn contains(&self, owner: Address, key: U256) -> bool {
        (owner == self.fee_token && key == self.fee_balance_slot)
            || (owner == super::ACCOUNT_KEYCHAIN_ADDRESS && self.keychain_limit_slot == Some(key))
    }
}

scoped_thread_local!(static NON_CREDITABLE_SLOTS: NonCreditableSlots);

pub(crate) fn with_non_creditable_slots<T>(slots: &NonCreditableSlots, f: impl FnOnce() -> T) -> T {
    NON_CREDITABLE_SLOTS.set(slots, f)
}

pub(crate) fn is_non_creditable_slot(owner: Address, key: U256) -> bool {
    NON_CREDITABLE_SLOTS.is_set() && NON_CREDITABLE_SLOTS.with(|slots| slots.contains(owner, key))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountingError {
    OutOfGas,
    Fatal,
}

/// Minimal state and gas interface shared by opcode and precompile SSTORE paths.
pub(crate) trait StorageCreditsBackend {
    fn gas_params(&self) -> GasParams;
    fn remaining_gas(&self) -> u64;
    fn charge_gas(&mut self, gas: u64) -> core::result::Result<(), AccountingError>;
    fn sload_raw(
        &mut self,
        address: Address,
        key: U256,
        skip_cold: bool,
    ) -> core::result::Result<StateLoad<U256>, AccountingError>;
    fn sstore_raw(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> core::result::Result<StateLoad<SStoreResult>, AccountingError>;
    fn tload_raw(&mut self, address: Address, key: U256) -> U256;
    fn tstore_raw(&mut self, address: Address, key: U256, value: U256);

    fn is_non_creditable_slot(&self, _owner: Address, _key: U256) -> bool {
        false
    }

    fn storage_credit_minting_enabled(&self) -> bool {
        true
    }
}

/// Applies TIP-1060 bookkeeping after a storage write has been journaled.
pub(crate) fn account_storage_write<B: StorageCreditsBackend>(
    backend: &mut B,
    owner: Address,
    key: Option<U256>,
    state_load: &StateLoad<SStoreResult>,
) -> core::result::Result<(), AccountingError> {
    let present_zero = state_load.data.present_value.is_zero();
    let new_zero = state_load.data.new_value.is_zero();
    if present_zero == new_zero || owner == STORAGE_CREDITS_ADDRESS {
        return Ok(());
    }

    let gas_params = backend.gas_params();
    backend.charge_gas(gas_params.warm_storage_read_cost())?;

    let account_slot = StorageCredits::slot(owner);
    let additional_cold_cost = gas_params.cold_storage_additional_cost();
    let skip_cold = backend.remaining_gas() < additional_cold_cost;
    let credit_load = backend.sload_raw(STORAGE_CREDITS_ADDRESS, account_slot, skip_cold)?;
    if credit_load.is_cold {
        backend.charge_gas(additional_cold_cost)?;
    }
    let mut credit = u64::try_from(credit_load.data).map_err(|_| AccountingError::Fatal)?;

    let mut balance_changed = false;
    if !present_zero && new_zero {
        if key.is_some_and(|slot| backend.is_non_creditable_slot(owner, slot)) {
            return Ok(());
        }
        if backend.storage_credit_minting_enabled() {
            credit = credit.saturating_add(1);
            balance_changed = true;
        }
    } else {
        let mut transient_state =
            TransientState::try_from(backend.tload_raw(STORAGE_CREDITS_ADDRESS, account_slot))
                .map_err(|_| AccountingError::Fatal)?;

        match transient_state.mode {
            CreditMode::Direct if credit > 0 && transient_state.budget > 0 => {
                credit -= 1;
                balance_changed = true;
                if transient_state.budget != u64::MAX {
                    transient_state.budget -= 1;
                    backend.tstore_raw(
                        STORAGE_CREDITS_ADDRESS,
                        account_slot,
                        transient_state.into(),
                    );
                }
            }
            CreditMode::Direct | CreditMode::Preserve => {
                backend.charge_gas(STORAGE_CREDIT_VALUE)?;
            }
            CreditMode::Refund => {
                backend.charge_gas(STORAGE_CREDIT_VALUE)?;
                transient_state.pending_refunds = transient_state.pending_refunds.saturating_add(1);
                backend.tstore_raw(
                    STORAGE_CREDITS_ADDRESS,
                    account_slot,
                    transient_state.into(),
                );
            }
        }
    }

    if balance_changed {
        let credit_store =
            backend.sstore_raw(STORAGE_CREDITS_ADDRESS, account_slot, U256::from(credit))?;
        if credit_store.data.new_values_changes_present()
            && credit_store.data.is_original_eq_present()
        {
            backend.charge_gas(gas_params.sstore_reset_without_cold_load_cost())?;
        }
    }

    Ok(())
}

struct OpcodeStorageCreditsBackend<'a, H> {
    host: &'a mut H,
    gas: &'a mut Gas,
}

impl<H: Host> StorageCreditsBackend for OpcodeStorageCreditsBackend<'_, H> {
    fn gas_params(&self) -> GasParams {
        self.host.gas_params().clone()
    }

    fn remaining_gas(&self) -> u64 {
        self.gas.remaining()
    }

    fn charge_gas(&mut self, gas: u64) -> core::result::Result<(), AccountingError> {
        self.gas
            .record_cost(gas)
            .then_some(())
            .ok_or(AccountingError::OutOfGas)
    }

    fn sload_raw(
        &mut self,
        address: Address,
        key: U256,
        skip_cold: bool,
    ) -> core::result::Result<StateLoad<U256>, AccountingError> {
        self.host
            .load_account_info_skip_cold_load(address, false, false)
            .map_err(|_| AccountingError::Fatal)?;
        self.host
            .sload_skip_cold_load(address, key, skip_cold)
            .map_err(|error| match error {
                revm::context_interface::host::LoadError::ColdLoadSkipped => {
                    AccountingError::OutOfGas
                }
                revm::context_interface::host::LoadError::DBError => AccountingError::Fatal,
            })
    }

    fn sstore_raw(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> core::result::Result<StateLoad<SStoreResult>, AccountingError> {
        self.host
            .sstore_skip_cold_load(address, key, value, false)
            .map_err(|_| AccountingError::Fatal)
    }

    fn tload_raw(&mut self, address: Address, key: U256) -> U256 {
        self.host.tload(address, key)
    }

    fn tstore_raw(&mut self, address: Address, key: U256, value: U256) {
        self.host.tstore(address, key, value);
    }
}

pub(crate) fn account_opcode_storage_write<H: Host>(
    host: &mut H,
    gas: &mut Gas,
    owner: Address,
    state_load: &StateLoad<SStoreResult>,
) -> core::result::Result<(), AccountingError> {
    account_storage_write(
        &mut OpcodeStorageCreditsBackend { host, gas },
        owner,
        None,
        state_load,
    )
}

impl TryFrom<U256> for TransientState {
    type Error = TempoPrecompileError;

    fn try_from(value: U256) -> Result<Self> {
        let limbs = value.as_limbs();
        Ok(Self {
            mode: (limbs[0] as u8).try_into()?,
            budget: limbs[1],
            pending_refunds: limbs[3],
        })
    }
}

impl From<TransientState> for U256 {
    fn from(value: TransientState) -> Self {
        Self::from_limbs([value.mode as u64, value.budget, 0, value.pending_refunds])
    }
}

/// Persistent balances and transaction-local credit policy.
pub struct StorageCredits {
    address: Address,
    storage: StorageCtx,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StorageCreditDeltas(BTreeMap<Address, u64>);

impl StorageCreditDeltas {
    pub(crate) fn new() -> Self {
        Self(BTreeMap::default())
    }

    pub(crate) fn credit_slots(&mut self, user: Address, slots: u64) {
        if slots == 0 {
            return;
        }

        self.0
            .entry(user)
            .and_modify(|total| *total = total.saturating_add(slots))
            .or_insert(slots);
    }

    pub(crate) fn flush(
        self,
        mut apply: impl FnMut(Address, u64) -> Result<()>,
    ) -> Result<()> {
        for (user, slots) in self.0 {
            apply(user, slots)?;
        }
        Ok(())
    }
}

impl StorageCredits {
    pub fn new() -> Self {
        Self {
            address: STORAGE_CREDITS_ADDRESS,
            storage: StorageCtx::default(),
        }
    }

    pub fn balance_of(&self, account: Address) -> Result<u64> {
        self.handler::<u64>(account).read()
    }

    pub fn mode_of(&self, account: Address) -> Result<CreditMode> {
        self.credit_state_of(account).map(|state| state.mode)
    }

    pub fn budget_of(&self, account: Address) -> Result<u64> {
        self.credit_state_of(account).map(|state| state.budget)
    }

    pub fn set_mode(&mut self, account: Address, mode: IStorageCredits::Mode) -> Result<()> {
        let mode = CreditMode::try_from(mode)?;
        let budget = if mode == CreditMode::Direct {
            u64::MAX
        } else {
            0
        };
        self.write_mode_with_budget(account, mode, budget)
    }

    pub fn set_budget(&mut self, account: Address, budget: u64) -> Result<()> {
        self.write_mode_with_budget(account, CreditMode::Direct, budget)
    }

    pub(crate) fn preserve(&mut self, account: Address) -> Result<()> {
        self.write_mode_with_budget(account, CreditMode::Preserve, 0)
    }

    pub(crate) fn track_minted_credits<T>(
        &self,
        account: Address,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<(T, u64)> {
        if !StorageCtx.spec().is_t7() {
            return f().map(|value| (value, 0));
        }

        let before = self.balance_of(account)?;
        let value = f()?;
        let after = self.balance_of(account)?;
        if after < before {
            return Err(TempoPrecompileError::Fatal(format!(
                "storage credit operation for {account} consumed credits"
            )));
        }
        Ok((value, after - before))
    }

    pub(crate) fn with_budget<T>(
        &mut self,
        account: Address,
        limit: u64,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<(T, i128)> {
        if !StorageCtx.spec().is_t7() {
            return f().map(|value| (value, 0));
        }

        if limit == 0 {
            let before = self.balance_of(account)?;
            let value = f()?;
            let after = self.balance_of(account)?;
            return Ok((value, i128::from(after) - i128::from(before)));
        }

        self.set_budget(account, limit)?;
        let before = self.balance_of(account)?;
        let result = f();
        let after = self.balance_of(account)?;
        let delta = i128::from(after) - i128::from(before);
        let pending_refunds = self.credit_state_of(account)?.pending_refunds;
        self.write_credit_state_of(
            account,
            TransientState {
                mode: CreditMode::Preserve,
                budget: 0,
                pending_refunds,
            },
        )?;
        result.map(|value| (value, delta))
    }

    fn write_mode_with_budget(
        &mut self,
        account: Address,
        mode: CreditMode,
        budget: u64,
    ) -> Result<()> {
        let mut state = self.credit_state_of(account)?;
        state.mode = mode;
        state.budget = budget;
        self.write_credit_state_of(account, state)
    }

    #[inline]
    pub fn slot(account: Address) -> U256 {
        U256::from_be_bytes(account.into_word().0)
    }

    #[inline]
    fn handler<T: StorableType>(&self, account: Address) -> T::Handler {
        T::handle(Self::slot(account), LayoutCtx::FULL, self.address)
    }

    #[inline]
    pub(crate) fn credit_state_of(&self, account: Address) -> Result<TransientState> {
        self.handler::<U256>(account).t_read()?.try_into()
    }

    #[inline]
    pub(crate) fn write_credit_state_of(
        &mut self,
        account: Address,
        state: TransientState,
    ) -> Result<()> {
        self.handler::<U256>(account).t_write(state.into())
    }
}

impl ContractStorage for StorageCredits {
    fn address(&self) -> Address {
        self.address
    }

    fn storage(&self) -> &StorageCtx {
        &self.storage
    }

    fn storage_mut(&mut self) -> &mut StorageCtx {
        &mut self.storage
    }
}

impl Precompile for StorageCredits {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        if !self.storage.spec().is_t7() {
            let selector = calldata
                .get(..4)
                .and_then(|bytes| bytes.try_into().ok())
                .unwrap_or([0; 4]);
            return unknown_selector(selector, 0);
        }

        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(
            calldata,
            |data| {
                IStorageCredits::IStorageCreditsCalls::abi_decode_with_config(
                    data,
                    super::abi_decoder_config(),
                )
            },
            |call| match call {
                IStorageCredits::IStorageCreditsCalls::balanceOf(call) => {
                    view(call, |call| self.balance_of(call.account))
                }
                IStorageCredits::IStorageCreditsCalls::modeOf(call) => {
                    view(call, |call| self.mode_of(call.account).map(Into::into))
                }
                IStorageCredits::IStorageCreditsCalls::budgetOf(call) => {
                    view(call, |call| self.budget_of(call.account))
                }
                IStorageCredits::IStorageCreditsCalls::setMode(call) => {
                    mutate_void(call, msg_sender, |sender, call| {
                        self.set_mode(sender, call.newMode)
                    })
                }
                IStorageCredits::IStorageCreditsCalls::setBudget(call) => {
                    mutate_void(call, msg_sender, |sender, call| {
                        self.set_budget(sender, call.credits)
                    })
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::sol_types::SolCall;

    use crate::tempo::hardfork::TempoHardfork;
    use crate::tempo::precompile::test_utils::TestStorageProvider;

    #[test]
    fn slot_is_left_padded_account_address() {
        let account = Address::repeat_byte(0x11);
        assert_eq!(
            StorageCredits::slot(account),
            U256::from_be_slice(account.as_slice())
        );
    }

    #[test]
    fn mode_and_budget_are_transaction_local() {
        let account = Address::repeat_byte(0x12);
        let mut provider = TestStorageProvider::new(TempoHardfork::T7);

        StorageCtx::enter(&mut provider, || {
            let mut credits = StorageCredits::new();
            assert_eq!(credits.mode_of(account)?, CreditMode::Refund);
            assert_eq!(credits.budget_of(account)?, 0);

            credits.set_mode(account, IStorageCredits::Mode::Direct)?;
            assert_eq!(credits.mode_of(account)?, CreditMode::Direct);
            assert_eq!(credits.budget_of(account)?, u64::MAX);

            credits.set_budget(account, 0)?;
            assert_eq!(credits.mode_of(account)?, CreditMode::Direct);
            assert_eq!(credits.budget_of(account)?, 0);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn precompile_is_t7_gated() {
        let call = IStorageCredits::balanceOfCall {
            account: Address::ZERO,
        };
        let mut provider = TestStorageProvider::new(TempoHardfork::T6);
        let output = StorageCtx::enter(&mut provider, || {
            StorageCredits::new().call(&call.abi_encode(), Address::ZERO)
        })
        .unwrap();
        assert!(output.reverted);
    }
}
