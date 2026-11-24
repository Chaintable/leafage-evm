//! HyperEVM.
//!
//! HyperEVM's execution rules are mainnet's, so this module reuses the mainnet
//! transaction environment and EVM builder. What it adds is the chain's read
//! precompiles: `0x800..=0x814` are backed by HyperCore state that an archive
//! cannot replay, so when forwarding is configured they resolve through an
//! upstream node instead of looking like calls to empty accounts.

mod api;
mod hl_read_precompile;

pub(crate) use api::HyperEvmConfig;
