//! Wire and on-disk account formats.
//!
//! A chain's account format is fixed by its evm type and selected explicitly;
//! nothing here guesses the format from the bytes. A diff or disk record of
//! the other format fails to decode (the RLP list lengths differ), so a
//! mismatched configuration surfaces as an error instead of wrong state.

use crate::account::{AccountExt, BalanceView, BlockStateUpdate, StoredAccount};
use crate::blast::{BlastAccountExt, BlastBlockStorageDiff, BlastSlimAccount};
use crate::primitives::{H256, KECCAK256_EMPTY};
use crate::storage::{BlockStorageDiff, NewAccount, SlimAccount};
use alloy_rlp::{Decodable, Encodable};

/// Account format of a chain's state diffs and disk records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StateDiffCodec {
    /// `{balance, nonce, code_hash}` accounts.
    #[default]
    Standard,
    /// Blast raw yield accounts, see [`crate::BlastNewAccount`].
    BlastV1,
}

/// Decodes an RLP state diff of the given format, advancing `buf` past it
/// like [`Decodable::decode`].
pub fn decode_state_diff(
    codec: StateDiffCodec,
    buf: &mut &[u8],
) -> Result<BlockStateUpdate, alloy_rlp::Error> {
    Ok(match codec {
        StateDiffCodec::Standard => BlockStorageDiff::<NewAccount>::decode(buf)?.into(),
        StateDiffCodec::BlastV1 => BlastBlockStorageDiff::decode(buf)?.into(),
    })
}

/// Encodes the on-disk value of an account. Standard accounts keep the
/// existing `rlp([balance, nonce, code_hash])` bytes.
pub fn encode_stored_account(account: &StoredAccount) -> Vec<u8> {
    let mut buf = Vec::new();
    match account.balance_view() {
        BalanceView::Standard(balance) => SlimAccount {
            balance,
            nonce: account.nonce(),
            code_hash: account.code_hash(),
        }
        .encode(&mut buf),
        BalanceView::Blast(ext) => BlastSlimAccount {
            nonce: account.nonce(),
            flags: ext.flags,
            fixed: ext.fixed,
            shares: ext.shares,
            remainder: ext.remainder,
            code_hash: account.code_hash(),
        }
        .encode(&mut buf),
    }
    buf
}

/// Decodes an on-disk account value written by [`encode_stored_account`].
/// The caller handles the empty value archive backends use as a deletion
/// marker before calling this.
pub fn decode_stored_account(
    codec: StateDiffCodec,
    mut bytes: &[u8],
) -> Result<StoredAccount, alloy_rlp::Error> {
    Ok(match codec {
        StateDiffCodec::Standard => {
            let slim = SlimAccount::decode(&mut bytes)?;
            StoredAccount::standard(
                slim.balance,
                slim.nonce,
                normalize_code_hash(slim.code_hash),
            )
        }
        StateDiffCodec::BlastV1 => {
            let slim = BlastSlimAccount::decode(&mut bytes)?;
            StoredAccount::with_ext(
                slim.nonce,
                normalize_code_hash(slim.code_hash),
                AccountExt::Blast(BlastAccountExt {
                    flags: slim.flags,
                    fixed: slim.fixed,
                    shares: slim.shares,
                    remainder: slim.remainder,
                }),
            )
        }
    })
}

/// A zero code hash on disk means "no code".
fn normalize_code_hash(code_hash: H256) -> H256 {
    if code_hash.is_zero() {
        KECCAK256_EMPTY.0.into()
    } else {
        code_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::{hex, U256};
    use crate::BlastNewAccount;

    fn blast_account(flags: u8) -> StoredAccount {
        StoredAccount::with_ext(
            7,
            H256::repeat_byte(4),
            AccountExt::Blast(BlastAccountExt {
                flags,
                fixed: U256::from(11),
                shares: U256::from(13),
                remainder: U256::from(17),
            }),
        )
    }

    /// Standard disk bytes must stay identical to the pre-existing
    /// `rlp(SlimAccount)` encoding.
    #[test]
    fn standard_disk_bytes_are_unchanged() {
        let account = StoredAccount::standard(U256::from(100), 1, H256::repeat_byte(0xaa));
        let bytes = encode_stored_account(&account);
        assert_eq!(hex::encode(&bytes), format!("e36401a0{}", "aa".repeat(32)));
        let mut legacy = Vec::new();
        SlimAccount {
            balance: U256::from(100),
            nonce: 1,
            code_hash: H256::repeat_byte(0xaa),
        }
        .encode(&mut legacy);
        assert_eq!(bytes, legacy);
        assert_eq!(
            decode_stored_account(StateDiffCodec::Standard, &bytes).unwrap(),
            account
        );
    }

    #[test]
    fn blast_disk_round_trip() {
        for flags in [0, 1, 2, 7] {
            let account = blast_account(flags);
            let bytes = encode_stored_account(&account);
            assert_eq!(
                decode_stored_account(StateDiffCodec::BlastV1, &bytes).unwrap(),
                account
            );
        }
    }

    #[test]
    fn disk_record_of_the_other_format_is_rejected() {
        let standard =
            encode_stored_account(&StoredAccount::standard(U256::from(1), 1, H256::ZERO));
        let blast = encode_stored_account(&blast_account(0));
        assert!(decode_stored_account(StateDiffCodec::BlastV1, &standard).is_err());
        assert!(decode_stored_account(StateDiffCodec::Standard, &blast).is_err());
    }

    #[test]
    fn zero_code_hash_decodes_as_empty_code() {
        let standard =
            encode_stored_account(&StoredAccount::standard(U256::from(1), 1, H256::ZERO));
        let decoded = decode_stored_account(StateDiffCodec::Standard, &standard).unwrap();
        assert_eq!(decoded.code_hash(), H256::from(KECCAK256_EMPTY.0));

        let blast = StoredAccount::with_ext(1, H256::ZERO, AccountExt::Blast(blast_ext()));
        let decoded =
            decode_stored_account(StateDiffCodec::BlastV1, &encode_stored_account(&blast)).unwrap();
        assert_eq!(decoded.code_hash(), H256::from(KECCAK256_EMPTY.0));
    }

    fn blast_ext() -> BlastAccountExt {
        BlastAccountExt {
            flags: 0,
            fixed: U256::ZERO,
            shares: U256::from(2),
            remainder: U256::from(3),
        }
    }

    fn standard_diff_bytes() -> Vec<u8> {
        let diff = BlockStorageDiff {
            hash: H256::repeat_byte(1),
            new_accounts: vec![NewAccount {
                address: H256::repeat_byte(3),
                balance: U256::from(10),
                nonce: 4,
                code_hash: H256::repeat_byte(5),
            }],
            ..Default::default()
        };
        let mut buf = Vec::new();
        diff.encode(&mut buf);
        buf
    }

    fn blast_diff_bytes() -> Vec<u8> {
        let diff = BlastBlockStorageDiff {
            hash: H256::repeat_byte(1),
            new_accounts: vec![BlastNewAccount {
                address: H256::repeat_byte(3),
                nonce: 4,
                flags: 0,
                fixed: U256::ZERO,
                shares: U256::from(2),
                remainder: U256::from(3),
                code_hash: H256::repeat_byte(5),
            }],
            ..Default::default()
        };
        let mut buf = Vec::new();
        diff.encode(&mut buf);
        buf
    }

    #[test]
    fn state_diff_decodes_with_its_own_codec() {
        let standard = decode_state_diff(
            StateDiffCodec::Standard,
            &mut standard_diff_bytes().as_slice(),
        )
        .unwrap();
        assert_eq!(
            standard.new_accounts[0].account,
            StoredAccount::standard(U256::from(10), 4, H256::repeat_byte(5))
        );

        let blast =
            decode_state_diff(StateDiffCodec::BlastV1, &mut blast_diff_bytes().as_slice()).unwrap();
        assert_eq!(blast.new_accounts[0].address, H256::repeat_byte(3));
        assert_eq!(
            blast.new_accounts[0].account,
            StoredAccount::with_ext(4, H256::repeat_byte(5), AccountExt::Blast(blast_ext()))
        );
    }

    #[test]
    fn state_diff_of_the_other_format_is_rejected() {
        assert!(decode_state_diff(
            StateDiffCodec::BlastV1,
            &mut standard_diff_bytes().as_slice()
        )
        .is_err());
        assert!(
            decode_state_diff(StateDiffCodec::Standard, &mut blast_diff_bytes().as_slice())
                .is_err()
        );
    }

    /// Absent payloads are not empty transitions; callers decide what an
    /// empty object means.
    #[test]
    fn empty_bytes_are_an_error() {
        assert!(decode_state_diff(StateDiffCodec::Standard, &mut &[][..]).is_err());
        assert!(decode_state_diff(StateDiffCodec::BlastV1, &mut &[][..]).is_err());
    }
}
