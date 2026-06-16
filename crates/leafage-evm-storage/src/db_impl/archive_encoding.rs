//! Shared encoding helpers for archive storage keys/values.
//!
//! Both MDBX and RocksDB archive backends use the same on-disk encoding,
//! so callers (e.g. bulk archive ingest) can pre-compute encoded entries
//! off the writer thread and feed them directly into a batch.
//!
//! # Versioned key ordering: descending (inverted) block height
//!
//! Account and storage keys embed the block height so the archive can answer
//! "state of this account/slot as of block H". The height is stored
//! **inverted** — `u64::MAX - block_num`, big-endian — so that for a fixed
//! `address` (account) or `address||slot` (storage) prefix, versions sort
//! **newest-first**. The point of this is to turn the historical read into a
//! *forward* seek, which is materially cheaper than the backward seek the
//! naive ascending layout forces. The full reasoning:
//!
//! ## The query, and why ordering direction matters
//!
//! A read at height `H` wants the **greatest version `≤ H`**. With keys laid
//! out per-slot, that target sits somewhere inside the slot's contiguous run
//! of versions, and the question is which direction you must seek to land on
//! it.
//!
//! * **Ascending `block_num` (the old layout)**: versions sort oldest→newest,
//!   so the wanted version is the largest key `≤ address‖slot‖H`. That is a
//!   `SeekForPrev` (backward seek).
//! * **Descending `MAX - block_num` (this layout)**: versions sort
//!   newest→oldest, so the wanted version is the smallest key
//!   `≥ address‖slot‖(MAX - H)`. That is a plain forward `Seek`.
//!
//! Correctness of the descending form rests on the identity
//! `(MAX - bn) ≥ (MAX - H)  ⟺  bn ≤ H`: the versions with `bn ≤ H` are exactly
//! those whose stored tail is `≥ (MAX - H)`, and the *smallest* such tail is
//! the *largest* `bn ≤ H` — which is what "smallest key ≥ target" returns.
//! Worked example, slot written at blocks 5, 10, 20, query `H = 15`:
//! stored tails are `MAX-20 < MAX-10 < MAX-5`; target is `MAX-15`; the first
//! tail `≥ MAX-15` is `MAX-10` → block 10. Correct (greatest ≤ 15).
//! A query at the tip lands on the very first entry of the prefix (the newest
//! version), the cheapest possible seek position.
//!
//! ## Why forward `Seek` beats `SeekForPrev` here
//!
//! 1. **It is the first-class RocksDB operation.** `SeekForPrev` does more
//!    internal work and reverse positioning over the merge heap is costlier.
//! 2. **The prefix bloom is actually usable.** The account/storage CFs carry a
//!    fixed-prefix extractor (32 bytes = address, 64 bytes = address+slot) with
//!    a prefix bloom. A forward `Seek(target)` may consult that bloom to skip
//!    SSTs whose prefix range can't contain the target — and the skip is sound
//!    because "smallest key ≥ target, within the target's prefix" means an SST
//!    with no key of that prefix holds nothing relevant.
//!
//!    `SeekForPrev` cannot use the bloom the same way, due to a **directional
//!    asymmetry**: "largest key ≤ target" can legitimately land on a key with a
//!    *smaller, different* prefix (the predecessor of a slot that has no
//!    version ≤ H lives in a neighbouring prefix). So "this SST has no key with
//!    the target prefix" does **not** imply "nothing relevant here", and the
//!    bloom's negative answer can't justify a skip without risking a wrong
//!    result. RocksDB therefore can't drive file-skipping off the prefix bloom
//!    on the reverse path (it falls back to total-order behaviour or restricts
//!    to same-prefix semantics). Forward seek removes that asymmetry, so the
//!    already-configured prefix bloom does its job.
//!
//! With descending keys the readers do a forward `Seek` to
//! `address(‖slot)‖(MAX - H)` and then prefix-check the landed key: if it still
//! shares the `address(‖slot)` prefix it's the answer; otherwise no version
//! `≤ H` exists (the slot was first written after H) → absent. Full-scan
//! iterators that reconstruct the latest state correspondingly take the
//! **first** record of each prefix (the newest) rather than the last.
//!
//! ## On-disk compatibility
//!
//! This is a **breaking on-disk format change** for the account/storage CFs of
//! archive databases: keys written by the old ascending layout are not
//! readable by the descending readers and vice versa. There is no in-DB format
//! marker, so an existing archive DB must be **rebuilt** (e.g. via
//! `archive-init`, or re-synced) after adopting this encoding. The
//! `BlockNumToBlockHash` index is unaffected — it keeps ascending
//! [`encode_block_num`], as its readers decode the raw block number.

use alloy_rlp::Encodable;
use leafage_evm_types::{NewAccount, SlimAccount, H256};

/// Length of the encoded `(address, block_num)` account key.
pub const ACCOUNT_KEY_LEN: usize = 64;
/// Length of the encoded `(address, storage_key, block_num)` storage key.
pub const STORAGE_KEY_LEN: usize = 96;

/// Encode a raw, **ascending** big-endian block number into the trailing
/// 32-byte slot. Used by the `BlockNumToBlockHash` index, whose readers decode
/// the raw number — do **not** use this for versioned account/storage key
/// tails (see [`encode_block_num_desc`]).
#[inline]
pub fn encode_block_num(block_num: u64) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[24..32].copy_from_slice(&block_num.to_be_bytes());
    bytes
}

/// Encode the **descending** (inverted) block height `u64::MAX - block_num`
/// into the trailing 32-byte slot, so versions of a key sort newest-first.
/// See the module docs for the full rationale.
#[inline]
pub fn encode_block_num_desc(block_num: u64) -> [u8; 32] {
    encode_block_num(u64::MAX - block_num)
}

/// Encode account key: `address(32) || (MAX - block_num)(32 BE)`.
///
/// The inverted height makes a forward `Seek(address || (MAX - H))` land on the
/// greatest version `≤ H`; see the module docs.
#[inline]
pub fn encode_account_key(address: H256, block_num: u64) -> [u8; ACCOUNT_KEY_LEN] {
    let mut key = [0u8; ACCOUNT_KEY_LEN];
    key[..32].copy_from_slice(address.as_slice());
    key[32..64].copy_from_slice(&encode_block_num_desc(block_num));
    key
}

/// Encode storage key: `address(32) || storage_key(32) || (MAX - block_num)(32 BE)`.
///
/// The inverted height makes a forward `Seek(address || slot || (MAX - H))` land
/// on the greatest version `≤ H`; see the module docs.
#[inline]
pub fn encode_storage_key(
    address: H256,
    storage_key: H256,
    block_num: u64,
) -> [u8; STORAGE_KEY_LEN] {
    let mut key = [0u8; STORAGE_KEY_LEN];
    key[..32].copy_from_slice(address.as_slice());
    key[32..64].copy_from_slice(storage_key.as_slice());
    key[64..96].copy_from_slice(&encode_block_num_desc(block_num));
    key
}

/// RLP-encode the slim form of an account (balance, nonce, code_hash) used
/// as the value for `AddressToAccount`.
#[inline]
pub fn encode_slim_account(account: NewAccount) -> Vec<u8> {
    let slim: SlimAccount = account.into();
    let mut buf = Vec::new();
    slim.encode(&mut buf);
    buf
}
