//! Base B20 token precompiles.
//!
//! B20 tokens are precompiles: no EVM bytecode is deployed at their address (only a marker
//! byte), and their behaviour is Rust code dispatched by address prefix. Their *state*,
//! however, lives in the EVM trie at the token address in an ERC-7201 namespaced layout,
//! so a node holding Base's state can execute them locally. See
//! `docs/BaseBerylPrecompiles.md`.
//!
//! This module is a port of Base reth's `base-common-precompiles`
//! (`/Users/cifer/base/crates/common/precompiles`). It covers the full token surface —
//! reads *and* writes — and, critically, meters gas per storage access rather than charging
//! a flat fee, so `estimateGas` over a B20 address agrees with a real Base node.
//!
//! Why a port rather than a dependency on Base's crate: `base-common-precompiles` is built
//! against revm 40 / alloy-evm 0.36, leafage against revm 36 / alloy-evm 0.29, and there is
//! no published `op-revm` for revm 40 (the latest, 20.0.0, requires revm ^38) — leafage's
//! whole Base execution path is built on `op_revm::OpEvm`. Linking Base's crate directly
//! would mean a 4-major revm bump plus replacing op-revm across every OP-stack chain
//! leafage supports.
//!
//! Not ported (still forwarded as `-39008`, see [`super::precompile::is_forwarded_registry`]):
//! the B20 factory, the activation registry, and the policy registry's *administrative*
//! dispatch. The policy registry's read path is ported here because every transfer consults
//! it.

mod abi;
mod dispatch;
mod error;
mod ids;
mod layout;
mod ops;
mod permit;
mod policy;
mod port;

pub use abi::{IB20, IB20Asset, IB20Stablecoin};
pub use dispatch::{calldata_gas_cost, dispatch, B20Outcome};
pub use error::{B20Error, Result};
pub use layout::{B20Store, PolicySlot, ASSET_MIN_DECIMALS, STABLECOIN_DECIMALS, WAD};
pub use policy::POLICY_REGISTRY;
pub use port::B20Port;

use alloy::primitives::Address;

/// The B20 variant discriminant is byte 10 of the token address: 0 = asset, 1 = stablecoin.
///
/// Mirrors Base's `B20Variant::from_address`.
pub fn is_asset_variant(address: &Address) -> bool {
    address.as_slice()[10] == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_discriminant_reads_byte_ten() {
        let mut a = [0u8; 20];
        a[0] = 0xb2;
        a[10] = 0;
        assert!(is_asset_variant(&Address::from(a)));
        a[10] = 1;
        assert!(!is_asset_variant(&Address::from(a)));
    }
}
