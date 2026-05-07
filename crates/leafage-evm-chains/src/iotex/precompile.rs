//! IoTeX 4 system protocol addresses (staking / rewarding / poll / rolldpos).
//!
//! These look like ordinary EOAs to revm — no bytecode — so the EVM happily
//! returns "0x" for any call. The IoTeX writer (iotex-core-cc) instead routes
//! such calls through chain-specific ABI dispatch (see
//! `api/web3server.go::callProtocolAddr`). To preserve correctness in leafage,
//! we treat them as "unsupported precompiles": [`IotexEvm::frame_init`] checks
//! `is_unsupported` and short-circuits with `ContextError::Custom("unsupported
//! precompile address: 0x...")`, which the existing
//! `ToJsonRpcError for EVMError` arm in `leafage-evm-rpc/src/api_impl/api_impl.rs`
//! converts into the DeBank-standard `-39008 UnsupportedPrecompile` JSON-RPC
//! error. nodex-proxy then retries against the IoTeX writer node.
//!
//! This is the IoTeX analogue of cosmos chains' unsupported precompile list
//! (cosmos: 0x800-0x806 staking/distribution/ICS20/bank/governance/slashing).

use revm::primitives::{address, Address};
use std::collections::HashSet;
use std::sync::LazyLock;

const STAKING: Address = address!("0x04c22afae6a03438b8fed74cb1cf441168df3f12");
const REWARDING: Address = address!("0xa576c141e5659137ddda4223d209d4744b2106be");
const POLL: Address = address!("0x166b743c2c1a57c93c2e2bc3e169d28bbb9f6da3");
const ROLLDPOS: Address = address!("0x041370e00a711cd81da1918f0e494459aadae50e");

pub static UNSUPPORTED_LIST: LazyLock<HashSet<Address>> =
    LazyLock::new(|| [STAKING, REWARDING, POLL, ROLLDPOS].into_iter().collect());

pub fn is_unsupported(addr: &Address) -> bool {
    UNSUPPORTED_LIST.contains(addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn detects_protocol_addrs() {
        for hex in [
            "0x04c22afae6a03438b8fed74cb1cf441168df3f12",
            "0xa576c141e5659137ddda4223d209d4744b2106be",
            "0x166b743c2c1a57c93c2e2bc3e169d28bbb9f6da3",
            "0x041370e00a711cd81da1918f0e494459aadae50e",
        ] {
            let addr = Address::from_str(hex).unwrap();
            assert!(is_unsupported(&addr), "should detect {hex}");
        }
    }

    #[test]
    fn ignores_random_addrs() {
        for hex in [
            "0x1234567890123456789012345678901234567890",
            "0x0000000000000000000000000000000000000001",
            "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        ] {
            let addr = Address::from_str(hex).unwrap();
            assert!(!is_unsupported(&addr), "should ignore {hex}");
        }
    }
}
