//! Moonbeam/Moonriver precompile addresses that leafage cannot execute locally.
//!
//! Moonbeam and Moonriver expose three classes of precompiles (addresses are
//! taken verbatim from `runtime/{moonbeam,moonriver}/src/precompiles.rs` in the
//! Moonbeam runtime; the two runtimes share an identical address map):
//!
//! * `0x01..=0x09`, `0x0b..=0x11` (BLS12-381) and `0x100` (P256) — the standard
//!   Ethereum-range precompiles. These are provided by revm's [`EthPrecompiles`]
//!   at the configured spec and run locally with no special handling.
//! * `0x400` (Sha3FIPS256) and `0x402` (ECRecoverPublicKey) — Frontier-custom,
//!   stateless precompiles that revm does not provide.
//! * `0x800..=0x81a` — Moonbeam-specific precompiles, each a thin EVM gateway in
//!   front of a Substrate pallet (parachain-staking, balances, governance, XCM,
//!   …). Their return value depends on Substrate runtime storage that lives
//!   outside the EVM state trie leafage queries, so they cannot be reproduced
//!   here. (`0x808` Batch and `0x80a` CallPermit are pure-EVM and *could* be
//!   implemented locally later; for now they forward too.)
//!
//! Two more groups must forward to stay faithful to the runtime:
//!
//! * `0x0a` (KZG point evaluation) — revm registers this at Cancun+, but
//!   Moonbeam's Ethereum-range precompiles stop at `0x09` and resume at `0x0b`;
//!   it never implements slot `0x0a`. Without forwarding, leafage would answer
//!   `0x0a` with revm's KZG precompile instead of the empty-account behavior a
//!   real Moonbeam node gives.
//! * `0x401`, `0x803`, `0x80e`, `0x80f` — `RemovedPrecompileAt` slots (Dispatch,
//!   Democracy, Council, TechCommittee). The runtime still recognizes these and
//!   `execute()` returns `revert("Removed precompile")`; leafage would otherwise
//!   treat them as ordinary empty accounts and succeed with empty output.
//!
//! Like cosmos (`0x800..=0x806`) and IoTeX (the 4 protocol addresses), we treat
//! every precompile leafage does not execute locally as an "unsupported
//! precompile": [`crate::moonbeam::MoonbeamEvm::frame_init`] short-circuits with
//! `ContextError::Custom("unsupported precompile address: 0x...")`, which the
//! `ToJsonRpcError for EVMError` arm in
//! `leafage-evm-rpc/src/api_impl/api_impl.rs` converts into the DeBank-standard
//! `-39008 UnsupportedPrecompile` JSON-RPC error. nodex-proxy then retries the
//! call against a real Moonbeam/Moonriver node.
//!
//! [`EthPrecompiles`]: revm::handler::EthPrecompiles

use revm::primitives::{address, Address};
use std::collections::HashSet;
use std::sync::LazyLock;

// KZG point evaluation: present in revm's standard set at Cancun+, absent on
// Moonbeam (its Ethereum-range precompiles skip slot 0x0a). Forwarded so leafage
// does not run KZG where the runtime exposes an empty account.
const KZG_POINT_EVALUATION: Address = address!("0x000000000000000000000000000000000000000a");

// Frontier-custom stateless precompiles (not in revm's standard set).
const SHA3_FIPS256: Address = address!("0x0000000000000000000000000000000000000400");
const ECRECOVER_PUBLIC_KEY: Address = address!("0x0000000000000000000000000000000000000402");

// RemovedPrecompileAt slots: the runtime still matches these and reverts with
// "Removed precompile" rather than behaving as an empty account.
const DISPATCH_REMOVED: Address = address!("0x0000000000000000000000000000000000000401");
const DEMOCRACY_REMOVED: Address = address!("0x0000000000000000000000000000000000000803");
const COUNCIL_REMOVED: Address = address!("0x000000000000000000000000000000000000080e");
const TECH_COMMITTEE_REMOVED: Address = address!("0x000000000000000000000000000000000000080f");

// Moonbeam-specific precompiles (0x800..=0x81a), each backed by a Substrate
// pallet. The removed slots in this range (0x803, 0x80e, 0x80f) are handled
// above alongside the other RemovedPrecompileAt tombstones.
const PARACHAIN_STAKING: Address = address!("0x0000000000000000000000000000000000000800");
const CROWDLOAN_REWARDS: Address = address!("0x0000000000000000000000000000000000000801");
const ERC20_BALANCES: Address = address!("0x0000000000000000000000000000000000000802");
const XTOKENS: Address = address!("0x0000000000000000000000000000000000000804");
const RELAY_ENCODER: Address = address!("0x0000000000000000000000000000000000000805");
const XCM_TRANSACTOR_V1: Address = address!("0x0000000000000000000000000000000000000806");
const AUTHOR_MAPPING: Address = address!("0x0000000000000000000000000000000000000807");
const BATCH: Address = address!("0x0000000000000000000000000000000000000808");
const RANDOMNESS: Address = address!("0x0000000000000000000000000000000000000809");
const CALL_PERMIT: Address = address!("0x000000000000000000000000000000000000080a");
const PROXY: Address = address!("0x000000000000000000000000000000000000080b");
const XCM_UTILS: Address = address!("0x000000000000000000000000000000000000080c");
const XCM_TRANSACTOR_V2: Address = address!("0x000000000000000000000000000000000000080d");
const TREASURY_COUNCIL_COLLECTIVE: Address =
    address!("0x0000000000000000000000000000000000000810");
const REFERENDA: Address = address!("0x0000000000000000000000000000000000000811");
const CONVICTION_VOTING: Address = address!("0x0000000000000000000000000000000000000812");
const PREIMAGE: Address = address!("0x0000000000000000000000000000000000000813");
const OPEN_TECH_COMMITTEE_COLLECTIVE: Address =
    address!("0x0000000000000000000000000000000000000814");
const PRECOMPILE_REGISTRY: Address = address!("0x0000000000000000000000000000000000000815");
const GMP: Address = address!("0x0000000000000000000000000000000000000816");
const XCM_TRANSACTOR_V3: Address = address!("0x0000000000000000000000000000000000000817");
const IDENTITY: Address = address!("0x0000000000000000000000000000000000000818");
const RELAY_DATA_VERIFIER: Address = address!("0x0000000000000000000000000000000000000819");
const PALLET_XCM: Address = address!("0x000000000000000000000000000000000000081a");

pub static UNSUPPORTED_LIST: LazyLock<HashSet<Address>> = LazyLock::new(|| {
    [
        KZG_POINT_EVALUATION,
        SHA3_FIPS256,
        ECRECOVER_PUBLIC_KEY,
        DISPATCH_REMOVED,
        DEMOCRACY_REMOVED,
        COUNCIL_REMOVED,
        TECH_COMMITTEE_REMOVED,
        PARACHAIN_STAKING,
        CROWDLOAN_REWARDS,
        ERC20_BALANCES,
        XTOKENS,
        RELAY_ENCODER,
        XCM_TRANSACTOR_V1,
        AUTHOR_MAPPING,
        BATCH,
        RANDOMNESS,
        CALL_PERMIT,
        PROXY,
        XCM_UTILS,
        XCM_TRANSACTOR_V2,
        TREASURY_COUNCIL_COLLECTIVE,
        REFERENDA,
        CONVICTION_VOTING,
        PREIMAGE,
        OPEN_TECH_COMMITTEE_COLLECTIVE,
        PRECOMPILE_REGISTRY,
        GMP,
        XCM_TRANSACTOR_V3,
        IDENTITY,
        RELAY_DATA_VERIFIER,
        PALLET_XCM,
    ]
    .into_iter()
    .collect()
});

pub fn is_unsupported(addr: &Address) -> bool {
    UNSUPPORTED_LIST.contains(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn detects_moonbeam_precompiles() {
        for hex in [
            "0x000000000000000000000000000000000000000a", // KZG slot (absent on Moonbeam)
            "0x0000000000000000000000000000000000000400", // Sha3FIPS256
            "0x0000000000000000000000000000000000000402", // ECRecoverPublicKey
            "0x0000000000000000000000000000000000000401", // Dispatch (removed → reverts)
            "0x0000000000000000000000000000000000000803", // Democracy (removed → reverts)
            "0x000000000000000000000000000000000000080e", // CouncilInstance (removed → reverts)
            "0x000000000000000000000000000000000000080f", // TechCommittee (removed → reverts)
            "0x0000000000000000000000000000000000000800", // ParachainStaking
            "0x0000000000000000000000000000000000000802", // Erc20Balances
            "0x0000000000000000000000000000000000000808", // Batch
            "0x000000000000000000000000000000000000080a", // CallPermit
            "0x0000000000000000000000000000000000000811", // Referenda
            "0x000000000000000000000000000000000000081a", // PalletXcm
        ] {
            let addr = Address::from_str(hex).unwrap();
            assert!(is_unsupported(&addr), "should detect {hex}");
        }
    }

    #[test]
    fn ignores_locally_executable_addrs() {
        for hex in [
            "0x0000000000000000000000000000000000000001", // ECRecover (revm-native)
            "0x0000000000000000000000000000000000000009", // Blake2F (revm-native)
            "0x000000000000000000000000000000000000000b", // BLS12-381 G1Add (revm-native)
            "0x0000000000000000000000000000000000000100", // P256Verify (revm-native)
            "0x1234567890123456789012345678901234567890", // regular contract
        ] {
            let addr = Address::from_str(hex).unwrap();
            assert!(!is_unsupported(&addr), "should ignore {hex}");
        }
    }
}
