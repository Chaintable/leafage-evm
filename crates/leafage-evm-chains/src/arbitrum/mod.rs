//! Arbitrum Orbit (Nitro) support: replicate the L1 data-posting cost
//! (posterGas) that Nitro folds into `eth_estimateGas`.
//!
//! Self-contained pure logic — slot derivation for the ArbOS pricing values
//! ([`arbos_state`]) and the posterGas estimation arithmetic ([`poster_gas`]).
//! The RPC `EvmExecutor`/`GasFeeHandler` wiring lives in `leafage-evm-rpc`.

pub mod arbos_state;
pub mod config;
pub mod poster_gas;

pub use config::ArbitrumEvmConfig;
