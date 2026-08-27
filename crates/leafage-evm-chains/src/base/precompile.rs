//! Base (Beryl upgrade) precompile address scheme.
//!
//! Before Beryl, Base's precompiles are identical to the OP stack and run
//! locally via revm's op precompiles. Beryl adds a set of stateful precompiles
//! (installed dynamically in Base reth via `install_with_observer`):
//!
//! * Fixed-address registries:
//!   - `B20Factory`         `0xB20F0000…0000`
//!   - `ActivationRegistry` `0x8453…0001`
//!   - `PolicyRegistry`     `0x8453…0002`
//! * B20 token contracts — a whole address *range*, not fixed: any address whose
//!   first byte is `0xb2` and bytes `[1..10]` are zero (`0xb2_00…00_<variant>`),
//!   created deterministically by the factory. These are dispatched in Base reth
//!   by a dynamic `PrecompileLookup` (`BerylLookup`) rather than enumerated.
//!
//! Their *code* is a Rust precompile (no EVM bytecode is deployed), but their
//! *state* lives in the EVM trie at the token/registry address. So leafage's
//! plain op EVM would treat them as empty accounts and return wrong results.
//!
//! Plan (staged):
//! * Stage 2 — port the B20 token *view* methods (read-only) + the `0xb2…`
//!   prefix lookup so `eth_call` serves them locally against state.
//! * Stage 3 — the registries/factory/policy/activation are forwarded as
//!   `-39008 UnsupportedPrecompile` (admin/write-heavy, rarely read), matching
//!   the moonbeam/cosmos pattern.

use revm::primitives::{address, Address};

/// `B20Factory` registry precompile.
pub const B20_FACTORY: Address = address!("0xB20F000000000000000000000000000000000000");
/// `ActivationRegistry` precompile.
pub const ACTIVATION_REGISTRY: Address = address!("0x8453000000000000000000000000000000000001");
/// `PolicyRegistry` precompile.
pub const POLICY_REGISTRY: Address = address!("0x8453000000000000000000000000000000000002");

/// First byte of every B20 token address.
const B20_PREFIX_BYTE: u8 = 0xb2;

/// Whether `address` has the structural B-20 token prefix
/// (`0xb2` followed by nine zero bytes); the variant discriminant is byte 10.
///
/// Mirrors Base reth `B20Variant::has_b20_prefix`.
pub fn has_b20_prefix(address: &Address) -> bool {
    let bytes = address.as_slice();
    bytes[0] == B20_PREFIX_BYTE && bytes[1..10] == [0u8; 9]
}

/// Registry/factory precompiles that leafage forwards (`-39008`) rather than
/// executing locally (Stage 3). B20 *tokens* are handled by the prefix lookup
/// (Stage 2), not this list.
pub fn is_forwarded_registry(address: &Address) -> bool {
    *address == B20_FACTORY
        || *address == ACTIVATION_REGISTRY
        || *address == POLICY_REGISTRY
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn detects_b20_prefix() {
        // asset (b021) and stablecoin (b022) example token addresses
        for hex in [
            "0xb20000000000000000000000000000000000b021",
            "0xb20000000000000000000000000000000000b022",
        ] {
            assert!(has_b20_prefix(&Address::from_str(hex).unwrap()), "{hex}");
        }
        // not a B20 token
        assert!(!has_b20_prefix(
            &Address::from_str("0x1234567890123456789012345678901234567890").unwrap()
        ));
    }

    #[test]
    fn detects_forwarded_registries() {
        assert!(is_forwarded_registry(&B20_FACTORY));
        assert!(is_forwarded_registry(&ACTIVATION_REGISTRY));
        assert!(is_forwarded_registry(&POLICY_REGISTRY));
        assert!(!is_forwarded_registry(
            &Address::from_str("0x0000000000000000000000000000000000000001").unwrap()
        ));
    }
}
