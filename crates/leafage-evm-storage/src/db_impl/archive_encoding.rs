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
//! ## Runtime toggle and on-disk compatibility
//!
//! The descending layout is **opt-in at runtime** via the process-global flag
//! set by [`set_inverted_block_encoding`] (driven by the
//! `--inverted-block-encoding` CLI option). When unset (the default), the
//! account/storage key tails use the legacy **ascending** [`encode_block_num`]
//! and the readers use `SeekForPrev` / last-record-per-prefix; when set, they
//! use [`encode_block_num_desc`] and forward `Seek` / first-record-per-prefix.
//!
//! The two layouts are **mutually unreadable**: keys written ascending are not
//! correctly read by the descending readers and vice versa, and there is no
//! in-DB marker. The operator is responsible for matching the flag to the
//! database — run with `--inverted-block-encoding` only against a DB built
//! (via `archive-init` / re-sync) with the same flag. The archive backend
//! opens a single DB per process, so the flag is a process-wide setting fixed
//! at startup.
//!
//! The `BlockNumToBlockHash` index is unaffected either way — it keeps
//! ascending [`encode_block_num`], as its readers decode the raw block number.

use alloy_rlp::{Decodable, Encodable};
use leafage_evm_types::{
    BalanceState, BlastSlimAccount, SlimAccount, StateDiffCodec, StoredAccount, H256,
    KECCAK256_EMPTY,
};
use std::sync::atomic::{AtomicBool, Ordering};

/// Length of the encoded `(address, block_num)` account key.
pub const ACCOUNT_KEY_LEN: usize = 64;
/// Length of the encoded `(address, storage_key, block_num)` storage key.
pub const STORAGE_KEY_LEN: usize = 96;

/// Process-global selector for the versioned key encoding. `false` (default) =
/// legacy ascending; `true` = descending/inverted. Set once at startup from the
/// `--inverted-block-encoding` CLI flag, before any archive read/write.
static INVERTED_BLOCK_ENCODING: AtomicBool = AtomicBool::new(false);

/// Set the process-wide versioned-key encoding. Call once at startup (before
/// opening the archive DB or encoding any key); the archive backend operates a
/// single DB per process in one mode for its lifetime.
#[inline]
pub fn set_inverted_block_encoding(inverted: bool) {
    INVERTED_BLOCK_ENCODING.store(inverted, Ordering::Relaxed);
}

/// Whether versioned account/storage keys use the descending (inverted) height
/// tail. Reads, writes, and full-scan iterators must all branch on this.
#[inline]
pub fn inverted_block_encoding() -> bool {
    INVERTED_BLOCK_ENCODING.load(Ordering::Relaxed)
}

/// Encode the version tail per the current [`inverted_block_encoding`] mode.
#[inline]
fn encode_version_tail(block_num: u64) -> [u8; 32] {
    if inverted_block_encoding() {
        encode_block_num_desc(block_num)
    } else {
        encode_block_num(block_num)
    }
}

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

/// Encode account key: `address(32) || version_tail(32 BE)`, where the tail is
/// ascending or descending per [`inverted_block_encoding`]. In inverted mode a
/// forward `Seek(address || (MAX - H))` lands on the greatest version `≤ H`; in
/// legacy mode a `SeekForPrev(address || H)` does. See the module docs.
#[inline]
pub fn encode_account_key(address: H256, block_num: u64) -> [u8; ACCOUNT_KEY_LEN] {
    let mut key = [0u8; ACCOUNT_KEY_LEN];
    key[..32].copy_from_slice(address.as_slice());
    key[32..64].copy_from_slice(&encode_version_tail(block_num));
    key
}

/// Encode storage key: `address(32) || storage_key(32) || version_tail(32 BE)`,
/// with the tail ascending or descending per [`inverted_block_encoding`].
#[inline]
pub fn encode_storage_key(
    address: H256,
    storage_key: H256,
    block_num: u64,
) -> [u8; STORAGE_KEY_LEN] {
    let mut key = [0u8; STORAGE_KEY_LEN];
    key[..32].copy_from_slice(address.as_slice());
    key[32..64].copy_from_slice(storage_key.as_slice());
    key[64..96].copy_from_slice(&encode_version_tail(block_num));
    key
}

/// Process-global account value codec, mirroring the `--state-diff-codec`
/// CLI option (same operator contract as [`set_inverted_block_encoding`]:
/// set once at startup, before any account read/write, and it must match the
/// database). `false` (default) = standard 3-item [`SlimAccount`]; `true` =
/// Blast 6-item [`BlastSlimAccount`]. Decoding is strict per this flag —
/// opening a DB of the other format fails on every account read instead of
/// silently returning wrong data.
static BLAST_ACCOUNT_FORMAT: AtomicBool = AtomicBool::new(false);

/// Set the process-wide account value codec from the configured
/// state-diff codec. Call once at startup, before opening any DB.
#[inline]
pub fn set_account_codec(codec: StateDiffCodec) {
    BLAST_ACCOUNT_FORMAT.store(matches!(codec, StateDiffCodec::BlastV1), Ordering::Relaxed);
}

/// The process-wide account value codec.
#[inline]
pub fn account_codec() -> StateDiffCodec {
    if BLAST_ACCOUNT_FORMAT.load(Ordering::Relaxed) {
        StateDiffCodec::BlastV1
    } else {
        StateDiffCodec::Standard
    }
}

/// RLP-encode an account for the `AddressToAccount` value: standard accounts
/// keep the exact legacy 3-item [`SlimAccount`] bytes, Blast accounts use the
/// 6-item [`BlastSlimAccount`] form. The variant is produced by the
/// codec-gated state-diff decoder, so it always matches [`account_codec`].
#[inline]
pub fn encode_stored_account(account: StoredAccount) -> Vec<u8> {
    let mut buf = Vec::new();
    match account.balance_state {
        BalanceState::Standard { balance } => {
            SlimAccount {
                balance,
                nonce: account.nonce,
                code_hash: account.code_hash,
            }
            .encode(&mut buf);
        }
        BalanceState::Blast {
            flags,
            fixed,
            shares,
            remainder,
        } => {
            BlastSlimAccount {
                nonce: account.nonce,
                flags,
                fixed,
                shares,
                remainder,
                code_hash: account.code_hash,
            }
            .encode(&mut buf);
        }
    }
    buf
}

/// Decode an `AddressToAccount` value, strictly in the configured
/// [`account_codec`] format. See [`decode_stored_account_with`].
#[inline]
pub fn decode_stored_account(bytes: &[u8]) -> Result<StoredAccount, alloy_rlp::Error> {
    decode_stored_account_with(account_codec(), bytes)
}

/// Decode an `AddressToAccount` value, strictly in the given format:
/// `Standard` only accepts the 3-item form, `BlastV1` only the 6-item form —
/// a record of the other shape is an error, never silently reinterpreted.
/// Normalizes a zero `code_hash` (written by external producers for codeless
/// accounts) to `KECCAK256_EMPTY`.
#[inline]
pub fn decode_stored_account_with(
    codec: StateDiffCodec,
    mut bytes: &[u8],
) -> Result<StoredAccount, alloy_rlp::Error> {
    let account = match codec {
        StateDiffCodec::Standard => {
            let slim = SlimAccount::decode(&mut bytes)?;
            StoredAccount {
                nonce: slim.nonce,
                code_hash: slim.code_hash,
                balance_state: BalanceState::Standard {
                    balance: slim.balance,
                },
            }
        }
        StateDiffCodec::BlastV1 => {
            let slim = BlastSlimAccount::decode(&mut bytes)?;
            StoredAccount {
                nonce: slim.nonce,
                code_hash: slim.code_hash,
                balance_state: BalanceState::Blast {
                    flags: slim.flags,
                    fixed: slim.fixed,
                    shares: slim.shares,
                    remainder: slim.remainder,
                },
            }
        }
    };
    Ok(normalize_code_hash(account))
}

#[inline]
fn normalize_code_hash(mut account: StoredAccount) -> StoredAccount {
    if account.code_hash.is_zero() {
        account.code_hash = KECCAK256_EMPTY.0.into();
    }
    account
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_types::U256;

    #[test]
    fn test_stored_account_roundtrip_and_strict_shape() {
        let standard = StoredAccount {
            nonce: 5,
            code_hash: H256::repeat_byte(0x11),
            balance_state: BalanceState::Standard {
                balance: U256::from(100),
            },
        };
        let bytes = encode_stored_account(standard.clone());
        // Standard bytes are the exact legacy SlimAccount encoding (locked
        // byte-for-byte by leafage-evm-types::test_slim_account_golden_bytes).
        let mut slim = Vec::new();
        SlimAccount {
            balance: U256::from(100),
            nonce: 5,
            code_hash: H256::repeat_byte(0x11),
        }
        .encode(&mut slim);
        assert_eq!(bytes, slim);
        assert_eq!(
            decode_stored_account_with(StateDiffCodec::Standard, &bytes).unwrap(),
            standard
        );
        // Strict: a 3-item record under the Blast codec is an error.
        assert!(decode_stored_account_with(StateDiffCodec::BlastV1, &bytes).is_err());

        let blast = StoredAccount {
            nonce: 7,
            code_hash: H256::repeat_byte(0x22),
            balance_state: BalanceState::Blast {
                flags: 2,
                fixed: U256::from(11),
                shares: U256::from(13),
                remainder: U256::from(17),
            },
        };
        let bytes = encode_stored_account(blast.clone());
        assert_eq!(
            decode_stored_account_with(StateDiffCodec::BlastV1, &bytes).unwrap(),
            blast
        );
        // Strict: a 6-item record under the standard codec is an error.
        assert!(decode_stored_account_with(StateDiffCodec::Standard, &bytes).is_err());
    }

    #[test]
    fn test_zero_code_hash_normalizes_to_keccak_empty() {
        for account in [
            StoredAccount {
                nonce: 1,
                code_hash: H256::ZERO,
                balance_state: BalanceState::Standard {
                    balance: U256::ZERO,
                },
            },
            StoredAccount {
                nonce: 1,
                code_hash: H256::ZERO,
                balance_state: BalanceState::Blast {
                    flags: 0,
                    fixed: U256::ZERO,
                    shares: U256::ZERO,
                    remainder: U256::ZERO,
                },
            },
        ] {
            let codec = match account.balance_state {
                BalanceState::Standard { .. } => StateDiffCodec::Standard,
                BalanceState::Blast { .. } => StateDiffCodec::BlastV1,
            };
            let bytes = encode_stored_account(account);
            let decoded = decode_stored_account_with(codec, &bytes).unwrap();
            assert_eq!(decoded.code_hash, H256::from(KECCAK256_EMPTY.0));
        }
    }

    #[test]
    fn test_deletion_sentinel_never_collides_with_real_encoding() {
        // Archive backends store deletions as empty bytes; any real account
        // encoding is a non-empty RLP list.
        let blast = StoredAccount {
            nonce: 0,
            code_hash: H256::ZERO,
            balance_state: BalanceState::Blast {
                flags: 0,
                fixed: U256::ZERO,
                shares: U256::ZERO,
                remainder: U256::ZERO,
            },
        };
        assert!(!encode_stored_account(blast).is_empty());
        let standard = StoredAccount {
            nonce: 0,
            code_hash: H256::ZERO,
            balance_state: BalanceState::Standard {
                balance: U256::ZERO,
            },
        };
        assert!(!encode_stored_account(standard).is_empty());
    }
}
