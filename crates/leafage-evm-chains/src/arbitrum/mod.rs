//! Arbitrum Orbit (Nitro) support: replicate the L1 data-posting cost
//! (posterGas) that Nitro folds into `eth_estimateGas`.
//!
//! Self-contained pure logic — slot derivation for the ArbOS pricing values
//! ([`arbos_state`]) and Arbitrum-specific EVM execution support ([`evm`]).
//! The RPC `EvmExecutor`/`GasFeeHandler` wiring lives in `leafage-evm-rpc`.

pub mod arbos_state;
pub mod config;
pub mod evm;
pub mod hardforks;
pub mod precompile;
mod stylus_prefix;
pub mod tx;

pub use config::ArbitrumEvmConfig;
pub use hardforks::ArbitrumHardfork;
