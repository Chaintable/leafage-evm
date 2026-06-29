//! Arbitrum Orbit (Nitro) support: replicate the L1 data-posting cost
//! (posterGas) that Nitro folds into `eth_estimateGas`.
//!
//! Self-contained pure logic — slot derivation for the ArbOS pricing values
//! ([`arbos_state`]) and the posterGas estimation arithmetic ([`poster_gas`]).
//! The RPC `EvmExecutor`/`GasFeeHandler` wiring lives in `leafage-evm-rpc`.

pub mod arbos_state;
pub mod config;
pub mod context;
pub mod evm;
pub mod hardforks;
pub mod poster_gas;
pub mod precompile;
pub mod tx;

pub use config::ArbitrumEvmConfig;
pub use hardforks::ArbitrumHardfork;
