//! Blast (chain 81457) specific types.
//!
//! Blast rewrites the EL account model: raw yield fields replace the
//! materialized balance, and the balance is derived at read time against the
//! sharePrice of the same state view. Everything Blast-specific lives here;
//! the generic structures it plugs into ([`BlockStorageDiff`],
//! [`StoredAccount`]/[`AccountExt`], [`StateDiffCodec`]) live in `storage`,
//! following the one-module-per-chain convention.

use crate::primitives::{H256, U256};
use crate::storage::{AccountExt, AccountUpdate, BlockStorageDiff, StoredAccount};
use alloy_rlp::{Decodable, Encodable};

#[cfg(doc)]
use crate::storage::StateDiffCodec;

/// Blast raw yield fields (the non-trie-root part of blast-geth's
/// `StateAccount`). Plain data with no invariant of its own, so fields stay
/// public. One definition shared by the wire account ([`BlastNewAccount`],
/// where it is flattened into the 7-item RLP list) and the internal
/// [`StoredAccount`] extension. The balance is derived at read time:
/// flags 0 (Automatic) -> `shares * sharePrice + remainder`, any other
/// flags -> `fixed`. Unknown flags are passed through, not rejected (the
/// read side mirrors blast-geth `Balance()`: anything non-zero returns
/// `fixed`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlastAccountExt {
    /// Yield mode: 0 = Automatic, 1 = Disabled, 2 = Claimable (passed through)
    pub flags: u8,
    pub fixed: U256,
    pub shares: U256,
    pub remainder: U256,
}

/// Blast (chain 81457) wire account. blast-geth stores raw yield fields
/// instead of a balance; the balance is derived at read time against the
/// sharePrice of the same state view.
///
/// The raw yield fields are the embedded [`BlastAccountExt`] — the same
/// struct the internal [`StoredAccount`] extension carries, so the wire and
/// internal representations share one definition. On the wire the ext's
/// fields are **flattened in place**, mirroring Chaintable/pipeline
/// `types.BlastNewAccount` exactly (7 items:
/// `[address, nonce, flags, fixed, shares, remainder, code_hash]`); the RLP
/// impls are hand-written below because a derived nested struct would encode
/// as a nested list and change the bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct BlastNewAccount {
    /// keccak256 of the account address
    pub address: H256,
    /// Account nonce
    pub nonce: u64,
    /// Blast raw yield fields, flattened on the wire
    pub ext: BlastAccountExt,
    /// code hash
    pub code_hash: H256,
}

impl BlastNewAccount {
    fn rlp_payload_length(&self) -> usize {
        self.address.length()
            + self.nonce.length()
            + self.ext.flags.length()
            + self.ext.fixed.length()
            + self.ext.shares.length()
            + self.ext.remainder.length()
            + self.code_hash.length()
    }
}

impl Encodable for BlastNewAccount {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        alloy_rlp::Header {
            list: true,
            payload_length: self.rlp_payload_length(),
        }
        .encode(out);
        self.address.encode(out);
        self.nonce.encode(out);
        self.ext.flags.encode(out);
        self.ext.fixed.encode(out);
        self.ext.shares.encode(out);
        self.ext.remainder.encode(out);
        self.code_hash.encode(out);
    }

    fn length(&self) -> usize {
        let payload_length = self.rlp_payload_length();
        payload_length + alloy_rlp::length_of_length(payload_length)
    }
}

impl Decodable for BlastNewAccount {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let payload = alloy_rlp::Header::decode_bytes(buf, true)?;
        let mut b = payload;
        let this = Self {
            address: Decodable::decode(&mut b)?,
            nonce: Decodable::decode(&mut b)?,
            ext: BlastAccountExt {
                flags: Decodable::decode(&mut b)?,
                fixed: Decodable::decode(&mut b)?,
                shares: Decodable::decode(&mut b)?,
                remainder: Decodable::decode(&mut b)?,
            },
            code_hash: Decodable::decode(&mut b)?,
        };
        // Same strictness as the derive: the list payload must be exactly
        // consumed — trailing items are an error, never ignored.
        if !b.is_empty() {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: payload.len(),
                got: payload.len() - b.len(),
            });
        }
        Ok(this)
    }
}

/// Blast wire state diff: the same [`BlockStorageDiff`] container carrying
/// Blast wire accounts (7 RLP items instead of 4). Mirrors
/// Chaintable/pipeline `types.BlastBlockStorageDiff` exactly.
pub type BlastBlockStorageDiff = BlockStorageDiff<BlastNewAccount>;

impl From<BlastNewAccount> for StoredAccount {
    fn from(account: BlastNewAccount) -> Self {
        StoredAccount::with_ext(
            account.nonce,
            account.code_hash,
            AccountExt::Blast(account.ext),
        )
    }
}

impl From<BlastNewAccount> for AccountUpdate {
    fn from(account: BlastNewAccount) -> Self {
        AccountUpdate {
            address: account.address,
            account: account.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        decode_state_diff, AccountStorageDiff, BalanceView, IndexValuePair, NewAccount, NewCode,
        StateDiffCodec,
    };

    fn h256(n: u8) -> H256 {
        let mut bytes = [0u8; 32];
        bytes[31] = n;
        H256::from(bytes)
    }

    fn blast_diff_fixture_value() -> BlastBlockStorageDiff {
        BlastBlockStorageDiff {
            hash: h256(1),
            parent_hash: h256(2),
            new_accounts: vec![BlastNewAccount {
                address: h256(3),
                nonce: 7,
                ext: BlastAccountExt {
                    flags: 2,
                    fixed: U256::from(11),
                    shares: U256::from(13),
                    remainder: U256::from(17),
                },
                code_hash: h256(4),
            }],
            deleted_accounts: vec![h256(5)],
            storage_diffs: vec![AccountStorageDiff {
                address: h256(6),
                diffs: vec![IndexValuePair {
                    index: h256(7),
                    value: U256::from(19),
                }],
            }],
            new_codes: vec![NewCode {
                code_hash: h256(8),
                code: vec![0xde, 0xad, 0xbe, 0xef].into(),
            }],
        }
    }

    /// Shared Go/Rust golden vector. The fixture bytes are produced by
    /// Chaintable/pipeline `types.TestBlastBlockStorageDiffRLP` from
    /// `types/testdata/blast_state_diff.rlp.hex` (PR #113); both codebases
    /// must decode and re-encode them identically.
    #[test]
    fn test_blast_state_diff_golden_vector() {
        let fixture = include_str!("../testdata/blast_state_diff.rlp.hex");
        let want_bytes = alloy::primitives::hex::decode(fixture.trim()).unwrap();

        let decoded = BlastBlockStorageDiff::decode(&mut want_bytes.as_slice()).unwrap();
        assert_eq!(decoded, blast_diff_fixture_value());

        let mut encoded = Vec::new();
        blast_diff_fixture_value().encode(&mut encoded);
        assert_eq!(encoded, want_bytes);
    }

    /// The two wire formats must reject each other. Account items differ in
    /// shape (4 vs 7 RLP items), so any diff carrying at least one account
    /// fails to decode under the wrong type (an account-less diff encodes
    /// identically in both formats and is semantically equal anyway).
    #[test]
    fn test_blast_and_standard_wire_reject_each_other() {
        let mut blast_bytes = Vec::new();
        blast_diff_fixture_value().encode(&mut blast_bytes);
        assert!(BlockStorageDiff::<NewAccount>::decode(&mut blast_bytes.as_slice()).is_err());

        let standard = BlockStorageDiff {
            hash: h256(1),
            parent_hash: h256(2),
            new_accounts: vec![NewAccount {
                address: h256(3),
                balance: U256::from(100),
                nonce: 7,
                code_hash: h256(4),
            }],
            ..Default::default()
        };
        let mut standard_bytes = Vec::new();
        standard.encode(&mut standard_bytes);
        assert!(BlastBlockStorageDiff::decode(&mut standard_bytes.as_slice()).is_err());
    }

    #[test]
    fn test_decode_state_diff_blast_v1() {
        let mut blast_bytes = Vec::new();
        blast_diff_fixture_value().encode(&mut blast_bytes);

        let update = decode_state_diff(StateDiffCodec::BlastV1, &blast_bytes).unwrap();
        assert_eq!(update.hash, h256(1));
        assert_eq!(update.deleted_accounts, vec![h256(5)]);
        assert_eq!(update.storage_diffs.len(), 1);
        assert_eq!(update.new_codes.len(), 1);
        let blast_ext = BlastAccountExt {
            flags: 2,
            fixed: U256::from(11),
            shares: U256::from(13),
            remainder: U256::from(17),
        };
        assert_eq!(
            update.new_accounts[0].account,
            StoredAccount::with_ext(7, h256(4), AccountExt::Blast(blast_ext.clone()))
        );
        // The invariant in action: an extended account has no readable
        // materialized balance; its balance view carries the raw fields.
        assert_eq!(update.new_accounts[0].account.standard_balance(), None);
        assert_eq!(
            update.new_accounts[0].account.balance_view(),
            BalanceView::Blast(&blast_ext)
        );
        assert!(decode_state_diff(StateDiffCodec::Standard, &blast_bytes).is_err());
    }
}
