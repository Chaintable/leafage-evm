//! Monad specific execution outcomes.

use leafage_evm_types::{DebankErrorCode, PreErrorCode};
use revm::context::result::HaltReason;

/// Halt reasons of a Monad transaction: the Ethereum ones plus
/// `EVMC_MONAD_RESERVE_BALANCE_VIOLATION` (`execute_message.cpp`), raised
/// when the transaction leaves an EOA below its reserve balance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MonadHaltReason {
    Base(HaltReason),
    /// `execution/monad/reserve_balance.cpp` `revert_transaction`: the
    /// transaction is reverted and all gas is consumed.
    ReserveBalanceViolation,
}

impl From<HaltReason> for MonadHaltReason {
    fn from(reason: HaltReason) -> Self {
        Self::Base(reason)
    }
}

impl From<MonadHaltReason> for DebankErrorCode {
    fn from(reason: MonadHaltReason) -> Self {
        match reason {
            MonadHaltReason::Base(base) => base.into(),
            MonadHaltReason::ReserveBalanceViolation => DebankErrorCode::BalanceExhausted,
        }
    }
}

impl From<MonadHaltReason> for PreErrorCode {
    fn from(reason: MonadHaltReason) -> Self {
        match reason {
            MonadHaltReason::Base(base) => base.into(),
            MonadHaltReason::ReserveBalanceViolation => PreErrorCode::InsufficientBalane,
        }
    }
}
