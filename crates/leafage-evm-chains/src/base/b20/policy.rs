//! PolicyRegistry read path.
//!
//! Every B20 transfer and mint consults the policy registry precompile at
//! `0x8453…0002`. Its state lives in the EVM trie like the tokens', so leafage serves it
//! locally rather than forwarding. Transcribed from Base reth
//! (`base/crates/common/precompiles/src/policy/storage.rs`).
//!
//! Only the read surface (`is_authorized`, `policy_exists`) is ported: those are the calls a
//! token makes. The registry's own administrative dispatch stays forwarded — see
//! `base::precompile::is_forwarded_registry`.
//!
//! The registry is versioned like the tokens (`policy/versions.rs`): Beryl knows only the
//! simple BLOCKLIST / ALLOWLIST types, while Cobalt adds the UNION / INTERSECT composites
//! (`policy/logic/v2.rs`), whose authorization evaluates their child policies live.

use alloy::primitives::{address, keccak256, Address, U256};

use super::error::{B20Error, Result};
use super::layout::{extract_u64, mapping_slot};
use super::port::B20Port;
use super::version::B20Version;

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
/// `children: Mapping<u64, Vec<u64>>` — slot 4 (Cobalt). Each value is a Solidity dynamic
/// array: the length at the mapping slot, the elements packed four `u64`s per word from
/// `keccak256(length_slot)`.
const OFF_CHILDREN: u64 = 4;

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
const UNION_TYPE: u8 = 2;
const INTERSECT_TYPE: u8 = 3;

/// Bit 255 of a packed policy word marks the policy as created.
const EXISTS_BIT: U256 = U256::from_limbs([0, 0, 0, 1u64 << 63]);

/// Type byte encoded in the high 8 bits of a policy ID.
const fn policy_id_type(policy_id: u64) -> u8 {
    (policy_id >> POLICY_ID_TYPE_SHIFT) as u8
}

/// Highest type byte `version`'s registry recognizes; anything above is a malformed ID.
fn max_policy_type(version: B20Version) -> u8 {
    if version.is_cobalt() {
        INTERSECT_TYPE
    } else {
        ALLOWLIST_TYPE
    }
}

/// Returns whether `account` is authorized under `policy_id`.
///
/// Mirrors Base's ordering exactly, including that malformed IDs are unauthorized rather
/// than reverting, and that both built-ins short-circuit before any storage read — the
/// built-in fast paths are why an unconfigured token's transfer costs one SLOAD, not two.
///
/// From Cobalt a composite is evaluated over its live child set, short-circuiting: UNION
/// stops at the first child that authorizes, INTERSECT at the first that does not. So an
/// empty (never-created) UNION authorizes nobody and an empty INTERSECT everybody.
pub fn is_authorized<P: B20Port>(
    port: &mut P,
    policy_id: u64,
    account: Address,
    version: B20Version,
) -> Result<bool> {
    if policy_id == ALWAYS_ALLOW_ID {
        return Ok(true);
    }
    if policy_id == ALWAYS_BLOCK_ID {
        return Ok(false);
    }
    if policy_id_type(policy_id) > max_policy_type(version) {
        return Ok(false);
    }
    match policy_id_type(policy_id) {
        // An unwritten membership slot reads false, which gives the right answer for both
        // simple types: an allowlist with no members authorizes nobody, a blocklist with no
        // members authorizes everybody.
        ALLOWLIST_TYPE => read_member(port, policy_id, account),
        BLOCKLIST_TYPE => Ok(!read_member(port, policy_id, account)?),
        UNION_TYPE => {
            for child in read_children(port, policy_id)? {
                if is_authorized(port, child, account, version)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        INTERSECT_TYPE => {
            for child in read_children(port, policy_id)? {
                if !is_authorized(port, child, account, version)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        // The malformed-ID guard above excluded every other type byte.
        _ => unreachable!("policy id type byte above the version's maximum was rejected above"),
    }
}

/// Returns whether `policy_id` names a built-in or previously created policy.
pub fn policy_exists<P: B20Port>(
    port: &mut P,
    policy_id: u64,
    version: B20Version,
) -> Result<bool> {
    if policy_id == ALWAYS_ALLOW_ID || policy_id == ALWAYS_BLOCK_ID {
        return Ok(true);
    }
    if policy_id_type(policy_id) > max_policy_type(version) {
        return Ok(false);
    }
    let packed = read_policy_word(port, policy_id)?;
    Ok(!(packed & EXISTS_BIT).is_zero())
}

/// Reads a composite's child IDs: one SLOAD for the length, then one per four children.
///
/// Mirrors Base's `Vec<u64>` load, including that a length above `u32::MAX` is an arithmetic
/// panic rather than a read of that many words.
fn read_children<P: B20Port>(port: &mut P, policy_id: u64) -> Result<Vec<u64>> {
    let len_slot = mapping_slot(
        ROOT_POLICY_REGISTRY.wrapping_add(U256::from(OFF_CHILDREN)),
        u64_key(policy_id),
    );
    let raw_len = port.sload(POLICY_REGISTRY, len_slot)?;
    if raw_len > U256::from(u32::MAX) {
        return Err(B20Error::under_overflow());
    }
    let len = raw_len.to::<usize>();
    let mut children = Vec::with_capacity(len);
    if len == 0 {
        return Ok(children);
    }
    let data_start = U256::from_be_bytes(keccak256(len_slot.to_be_bytes::<32>()).0);
    for word_index in 0..len.div_ceil(CHILDREN_PER_WORD) {
        let word = port.sload(
            POLICY_REGISTRY,
            data_start.wrapping_add(U256::from(word_index)),
        )?;
        let in_word = (len - word_index * CHILDREN_PER_WORD).min(CHILDREN_PER_WORD);
        for lane in 0..in_word {
            children.push(extract_u64(word, lane * 8));
        }
    }
    Ok(children)
}

/// Four `u64` child IDs share one storage word.
const CHILDREN_PER_WORD: usize = 4;

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
    fn composite_types_are_malformed_before_cobalt() {
        struct NoStorage;
        impl B20Port for NoStorage {
            fn sload(&mut self, _: Address, _: U256) -> Result<U256> {
                panic!("a malformed ID must not touch storage")
            }
            fn sstore(&mut self, _: Address, _: U256, _: U256) -> Result<()> {
                unreachable!()
            }
            fn emit_event(&mut self, _: Address, _: alloy::primitives::LogData) -> Result<()> {
                unreachable!()
            }
            fn has_code(&mut self, _: Address) -> Result<bool> {
                unreachable!()
            }
            fn deduct_gas(&mut self, _: u64) -> Result<()> {
                unreachable!()
            }
            fn caller(&self) -> Address {
                Address::ZERO
            }
            fn call_value(&self) -> U256 {
                U256::ZERO
            }
            fn chain_id(&self) -> u64 {
                0
            }
            fn timestamp(&self) -> U256 {
                U256::ZERO
            }
            fn is_static(&self) -> bool {
                false
            }
        }
        let union = (u64::from(UNION_TYPE) << 56) | 7;
        let intersect = (u64::from(INTERSECT_TYPE) << 56) | 7;
        for id in [union, intersect] {
            assert!(!is_authorized(&mut NoStorage, id, Address::ZERO, B20Version::V1).unwrap());
            assert!(!policy_exists(&mut NoStorage, id, B20Version::V1).unwrap());
        }
    }

    #[test]
    fn type_byte_is_the_high_eight_bits() {
        assert_eq!(policy_id_type(0), 0);
        assert_eq!(policy_id_type(1u64 << 56), 1);
        assert_eq!(policy_id_type(2u64 << 56), 2);
    }
}
