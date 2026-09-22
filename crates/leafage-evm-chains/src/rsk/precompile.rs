//! Rootstock (RSK) precompiles leafage does not execute locally.
//!
//! RSK's native contracts are Java code inside the node (rskj
//! `PrecompiledContracts`), not EVM bytecode, so to revm they look like
//! ordinary accounts without code and any call would "succeed" with empty
//! output. Their state is not reproducible either: the Bridge and REMASC keep
//! arbitrary-length serialized blobs in storage cells, which the pipeline state
//! diff (32-byte values) leaves out on purpose.
//!
//! * `0x…01000006` Bridge, `0x…01000008` REMASC — stateful native contracts.
//! * `0x…01000009` HDWalletUtils, `0x…01000010` BlockHeader,
//!   `0x…01000011` Environment — native contracts (BlockHeader reads the
//!   merged-mining fields of RSK headers, which leafage does not have).
//! * `0x…01000016` / `0x…01000017` — secp256k1 add / mul, stateless but not
//!   part of revm's standard set.
//! * `0x0a` (KZG), `0x0b..=0x11` (BLS12-381), `0x100` (P256VERIFY) — revm
//!   registers these from Cancun / Prague / Osaka on, but RSK's Ethereum-range
//!   precompiles stop at `0x09` (blake2f). They're forwarded so leafage never
//!   runs a precompile where a real RSK node sees an empty account, whatever
//!   `--spec-id` is configured.
//!
//! Like cosmos (`0x800..=0x806`), IoTeX (the 4 protocol addresses) and
//! moonbeam, we treat all of them as "unsupported precompiles":
//! [`crate::rsk::RskEvm::frame_init`] short-circuits with
//! `ContextError::Custom("unsupported precompile address: 0x...")`, which the
//! `ToJsonRpcError for EVMError` arm in
//! `leafage-evm-rpc/src/api_impl/api_impl.rs` converts into the DeBank-standard
//! `-39008 UnsupportedPrecompile` JSON-RPC error. nodex-proxy then retries the
//! call against a real RSK node. The check runs on every frame, so a regular
//! contract that calls the Bridge internally is forwarded as a whole.

use revm::precompile::{modexp, PrecompileSpecId, Precompiles};
use revm::primitives::hardfork::SpecId;
use revm::primitives::{address, Address};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

// Native contracts (rskj `PrecompiledContracts`).
const BRIDGE: Address = address!("0x0000000000000000000000000000000001000006");
const REMASC: Address = address!("0x0000000000000000000000000000000001000008");
const HD_WALLET_UTILS: Address = address!("0x0000000000000000000000000000000001000009");
const BLOCK_HEADER: Address = address!("0x0000000000000000000000000000000001000010");
const ENVIRONMENT: Address = address!("0x0000000000000000000000000000000001000011");
const SECP256K1_ADD: Address = address!("0x0000000000000000000000000000000001000016");
const SECP256K1_MUL: Address = address!("0x0000000000000000000000000000000001000017");

// Present in revm's standard set at Cancun+ / Prague+ / Osaka+, absent on RSK.
const KZG_POINT_EVALUATION: Address = address!("0x000000000000000000000000000000000000000a");
const BLS12_G1ADD: Address = address!("0x000000000000000000000000000000000000000b");
const BLS12_G1MSM: Address = address!("0x000000000000000000000000000000000000000c");
const BLS12_G2ADD: Address = address!("0x000000000000000000000000000000000000000d");
const BLS12_G2MSM: Address = address!("0x000000000000000000000000000000000000000e");
const BLS12_PAIRING: Address = address!("0x000000000000000000000000000000000000000f");
const BLS12_MAP_FP_TO_G1: Address = address!("0x0000000000000000000000000000000000000010");
const BLS12_MAP_FP2_TO_G2: Address = address!("0x0000000000000000000000000000000000000011");
const P256VERIFY: Address = address!("0x0000000000000000000000000000000000000100");

pub static UNSUPPORTED_LIST: LazyLock<HashSet<Address>> = LazyLock::new(|| {
    [
        BRIDGE,
        REMASC,
        HD_WALLET_UTILS,
        BLOCK_HEADER,
        ENVIRONMENT,
        SECP256K1_ADD,
        SECP256K1_MUL,
        KZG_POINT_EVALUATION,
        BLS12_G1ADD,
        BLS12_G1MSM,
        BLS12_G2ADD,
        BLS12_G2MSM,
        BLS12_PAIRING,
        BLS12_MAP_FP_TO_G1,
        BLS12_MAP_FP2_TO_G2,
        P256VERIFY,
    ]
    .into_iter()
    .collect()
});

/// The precompiles RSK shares with Ethereum, priced like rskj does.
///
/// `0x05` MODEXP keeps the EIP-198 formula (`BigIntegerModexp.getGasForData`,
/// divisor 20, no minimum); RSK did not take EIP-2565. The others cost the same
/// as on Ethereum: altbn128 is at the EIP-1108 prices (RSKIP137) and blake2f at
/// 1 gas per round (RSKIP153).
pub(crate) fn rsk_precompiles(spec: SpecId) -> &'static Precompiles {
    // One leaked set per precompile spec, like revm's own per-spec statics.
    static SETS: LazyLock<Mutex<HashMap<PrecompileSpecId, &'static Precompiles>>> =
        LazyLock::new(Default::default);
    let spec = PrecompileSpecId::from_spec_id(spec);
    let mut sets = SETS.lock().unwrap_or_else(|e| e.into_inner());
    sets.entry(spec).or_insert_with(|| {
        let mut precompiles = Precompiles::new(spec).clone();
        precompiles.extend([modexp::BYZANTIUM]);
        Box::leak(Box::new(precompiles))
    })
}

/// Every address rskj's `PrecompiledContracts.getContractForAddress` answers
/// for: the native contracts and `0x01..=0x09`. `EXTCODESIZE` / `EXTCODEHASH`
/// special-case them.
pub(crate) fn is_rsk_precompile(addr: &Address) -> bool {
    const NATIVE: [Address; 7] = [
        BRIDGE,
        REMASC,
        HD_WALLET_UTILS,
        BLOCK_HEADER,
        ENVIRONMENT,
        SECP256K1_ADD,
        SECP256K1_MUL,
    ];
    let bytes = addr.as_slice();
    (bytes[..19].iter().all(|b| *b == 0) && (1..=9).contains(&bytes[19])) || NATIVE.contains(addr)
}

pub fn is_unsupported(addr: &Address) -> bool {
    UNSUPPORTED_LIST.contains(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn detects_native_contracts() {
        for hex in [
            "0x0000000000000000000000000000000001000006",
            "0x0000000000000000000000000000000001000008",
            "0x0000000000000000000000000000000001000009",
            "0x0000000000000000000000000000000001000010",
            "0x0000000000000000000000000000000001000011",
            "0x0000000000000000000000000000000001000016",
            "0x0000000000000000000000000000000001000017",
        ] {
            let addr = Address::from_str(hex).unwrap();
            assert!(is_unsupported(&addr), "should detect {hex}");
        }
    }

    #[test]
    fn detects_ethereum_precompiles_rsk_does_not_have() {
        for hex in [
            "0x000000000000000000000000000000000000000a",
            "0x000000000000000000000000000000000000000b",
            "0x0000000000000000000000000000000000000011",
            "0x0000000000000000000000000000000000000100",
        ] {
            let addr = Address::from_str(hex).unwrap();
            assert!(is_unsupported(&addr), "should detect {hex}");
        }
    }

    /// `0x01..=0x09` are the precompiles RSK shares with Ethereum (blake2f
    /// included), so revm runs them locally.
    #[test]
    fn ignores_shared_precompiles_and_regular_addrs() {
        for n in 1u8..=9 {
            let addr = Address::with_last_byte(n);
            assert!(!is_unsupported(&addr), "should ignore precompile {addr}");
        }
        for hex in [
            "0x1234567890123456789012345678901234567890",
            // not a native contract: 0x…01000007 was never assigned
            "0x0000000000000000000000000000000001000007",
            "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        ] {
            let addr = Address::from_str(hex).unwrap();
            assert!(!is_unsupported(&addr), "should ignore {hex}");
        }
    }
}
