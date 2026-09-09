//! Monad mainnet (chain id 143) execution support.
//!
//! Everything here mirrors `monad-execution` (Category Labs). The chain runs a
//! standard Ethereum EVM (Cancun / Prague / Osaka, selected by block timestamp
//! through the `MONAD_x` revision table) with the following deviations:
//!
//! * gas refunds are always zero (`monad_transaction_gas.cpp`);
//! * pricing v1 (MONAD_SEVEN): cold account 10 000 / cold storage 8 000 and
//!   precompile multipliers (`traits.hpp`, `monad_precompiles_gas_cost_impl.cpp`);
//! * MIP-3 (MONAD_NINE): linear memory expansion `words / 2` with an 8 MB
//!   transaction-wide memory cap (`vm/runtime/types.hpp`);
//! * MIP-8 (MONAD_TEN): page based storage pricing (`vm/runtime/storage.cpp`,
//!   `state3/page_tracker.hpp`);
//! * contract code limit 128 KB, initcode limit 256 KB, no CREATE inside an
//!   EIP-7702 delegated account, EIP-7951 P256 precompile from MONAD_FOUR;
//! * stateful precompiles `0x1000` (staking) and `0x1001` (reserve balance);
//! * the reserve balance rule: a transaction leaving an EOA below
//!   `min(10 MON, pre-transaction balance)` is reverted;
//! * unused gas is not refunded, the sender pays the whole gas limit.

mod api;
mod evm;
mod gas;
mod handler;
mod hardforks;
mod page_tracker;
mod precompile;
mod reserve_balance;
mod result;
#[cfg(test)]
mod tests;

pub use api::{MonadContext, MonadEvm};
pub use hardforks::MonadHardfork;
pub use precompile::{
    MonadPrecompiles, RESERVE_BALANCE_CONTRACT_ADDRESS, STAKING_CONTRACT_ADDRESS,
};
pub use result::MonadHaltReason;

/// Monad mainnet chain id.
pub const MONAD_MAINNET_CHAIN_ID: u64 = 143;

/// Consensus level per-transaction gas limit (`monad-eth-block-policy`
/// `TFM_MAX_GAS_LIMIT`). Transactions above it are never included in a block,
/// so gas estimation must not exceed it.
pub const TFM_MAX_TX_GAS_LIMIT: u64 = 30_000_000;
