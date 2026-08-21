//! PolicyRegistry read path.
//!
//! Every B20 transfer and mint consults the policy registry precompile at
//! `0x8453…0002`. Its state lives in the EVM trie like the tokens', so leafage serves it
//! locally rather than forwarding. Transcribed from Base reth
//! (`base/crates/common/precompiles/src/policy/storage.rs`).
//!
//! Only the read surface (`is_authorized`, `policy_exists`, `get_policy_admin`) is ported:
//! those are the calls a token makes. The registry's own administrative dispatch stays
//! forwarded — see `base::precompile::is_forwarded_registry`.

use alloy::primitives::{address, Address, U256};

use super::error::Result;
use super::layout::mapping_slot;
use super::port::B20Port;

/// Singleton address of the `PolicyRegistry` precompile.
pub const POLICY_REGISTRY: Address = address!("0x8453000000000000000000000000000000000002");

/// `base.policy_registry` ERC-7201 namespace root.
pub const ROOT_POLICY_REGISTRY: U256 = U256::from_limbs([
    0x49dcaece71ba4a00,
    0x46c55c449dfd447e,
    0xfe3151dc68f90b39,
    0x00503aeb06982fa1,
]);

/// `policies: Mapping<u64, U256>` — slot 0 of the namespace.
const OFF_POLICIES: u64 = 0;
/// `members: Mapping<u64, Mapping<Address, bool>>` — slot 1.
const OFF_MEMBERS: u64 = 1;

/// Built-in policy that authorizes everyone.
///
/// Encoded as BLOCKLIST (type 0) with counter 0 — an empty blocklist allows all. This is
/// also the EVM zero default, so an uninitialized policy field means "allow", and the
/// fast-path below must return before touching storage.
pub const ALWAYS_ALLOW_ID: u64 = 0;
/// Built-in policy that rejects everyone: ALLOWLIST (type 1), counter 1, empty member set.
pub const ALWAYS_BLOCK_ID: u64 = (1u64 << POLICY_ID_TYPE_SHIFT) | 1;

const POLICY_ID_TYPE_SHIFT: usize = 56;
const BLOCKLIST_TYPE: u8 = 0;
const ALLOWLIST_TYPE: u8 = 1;

/// Bit 255 of a packed policy word marks the policy as created.
const EXISTS_BIT: U256 = U256::from_limbs([0, 0, 0, 1u64 << 63]);

/// Type byte encoded in the high 8 bits of a policy ID.
const fn policy_id_type(policy_id: u64) -> u8 {
    (policy_id >> POLICY_ID_TYPE_SHIFT) as u8
}

/// Returns whether `account` is authorized under `policy_id`.
///
/// Mirrors Base's ordering exactly, including that malformed IDs are unauthorized rather
/// than reverting, and that both built-ins short-circuit before any storage read — the
/// built-in fast paths are why an unconfigured token's transfer costs one SLOAD, not two.
pub fn is_authorized<P: B20Port>(port: &mut P, policy_id: u64, account: Address) -> Result<bool> {
    if policy_id_type(policy_id) > ALLOWLIST_TYPE {
        return Ok(false);
    }
    if policy_id == ALWAYS_ALLOW_ID {
        return Ok(true);
    }
    if policy_id == ALWAYS_BLOCK_ID {
        return Ok(false);
    }

    // An unwritten membership slot reads false, which gives the right answer for both
    // types: an allowlist with no members authorizes nobody, a blocklist with no members
    // authorizes everybody.
    let member = read_member(port, policy_id, account)?;
    Ok(match policy_id_type(policy_id) {
        ALLOWLIST_TYPE => member,
        BLOCKLIST_TYPE => !member,
        // The malformed-ID guard above excluded every other type byte.
        _ => unreachable!("policy id type byte > 1 was rejected above"),
    })
}

/// Returns whether `policy_id` names a built-in or previously created policy.
pub fn policy_exists<P: B20Port>(port: &mut P, policy_id: u64) -> Result<bool> {
    if policy_id_type(policy_id) > ALLOWLIST_TYPE {
        return Ok(false);
    }
    if policy_id == ALWAYS_ALLOW_ID || policy_id == ALWAYS_BLOCK_ID {
        return Ok(true);
    }
    let packed = read_policy_word(port, policy_id)?;
    Ok(!(packed & EXISTS_BIT).is_zero())
}

fn read_member<P: B20Port>(port: &mut P, policy_id: u64, account: Address) -> Result<bool> {
    let outer = mapping_slot(
        ROOT_POLICY_REGISTRY.wrapping_add(U256::from(OFF_MEMBERS)),
        u64_key(policy_id),
    );
    let slot = mapping_slot(outer, account.into_word());
    Ok(!port.sload(POLICY_REGISTRY, slot)?.is_zero())
}

fn read_policy_word<P: B20Port>(port: &mut P, policy_id: u64) -> Result<U256> {
    let slot = mapping_slot(
        ROOT_POLICY_REGISTRY.wrapping_add(U256::from(OFF_POLICIES)),
        u64_key(policy_id),
    );
    port.sload(POLICY_REGISTRY, slot)
}

/// A `u64` mapping key is left-padded to 32 bytes, like any value-type key.
fn u64_key(value: u64) -> alloy::primitives::B256 {
    alloy::primitives::B256::from(U256::from(value).to_be_bytes::<32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_root_matches_base() {
        assert_eq!(
            format!("{ROOT_POLICY_REGISTRY:#x}"),
            "0x503aeb06982fa1fe3151dc68f90b3946c55c449dfd447e49dcaece71ba4a00"
        );
    }

    #[test]
    fn builtin_ids_match_base_encoding() {
        assert_eq!(ALWAYS_ALLOW_ID, 0);
        assert_eq!(ALWAYS_BLOCK_ID, (1u64 << 56) | 1);
        assert_eq!(policy_id_type(ALWAYS_ALLOW_ID), BLOCKLIST_TYPE);
        assert_eq!(policy_id_type(ALWAYS_BLOCK_ID), ALLOWLIST_TYPE);
    }

    #[test]
    fn type_byte_is_the_high_eight_bits() {
        assert_eq!(policy_id_type(0), 0);
        assert_eq!(policy_id_type(1u64 << 56), 1);
        assert_eq!(policy_id_type(2u64 << 56), 2);
    }
}
