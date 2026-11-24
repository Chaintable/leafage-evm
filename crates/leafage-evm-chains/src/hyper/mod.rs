//! HyperEVM chain specifics.
//!
//! HyperEVM is EVM-equivalent to mainnet for execution, but it exposes the
//! HyperCore perps engine's state through a set of read-only precompiles in
//! `0x800..=0x814` that only exist on this chain. The state behind them is not
//! part of the EVM state, so it can only be read from HyperCore itself.
//!
//! [`read_precompiles`] owns everything about those precompiles that is chain
//! behaviour — the address range, the gas schedule, and how an answer becomes a
//! precompile result. Fetching the answer (from a HyperCore node, typically over
//! `eth_call`) is the RPC layer's job, through the [`read_precompiles::Resolver`]
//! trait.

pub mod read_precompiles;
