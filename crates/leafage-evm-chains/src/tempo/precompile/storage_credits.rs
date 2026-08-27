//! TIP-1060 storage credits precompile (T7+).

use alloy::primitives::{Address, U256};
use alloy::sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileResult};

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
