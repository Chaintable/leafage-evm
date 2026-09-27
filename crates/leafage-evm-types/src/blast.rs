//! Blast (chain 81457) account model.
//!
//! blast-geth replaces the account balance with raw yield fields
//! (`flags / fixed / shares / remainder`). The balance of an automatic-yield
//! account is `shares * sharePrice + remainder`, where sharePrice lives in the
//! storage of a predeploy and changes without touching any account leaf. The
//! raw fields are therefore carried end to end and the balance is derived at
//! read time against the sharePrice of the same state view.

use crate::primitives::{H256, U256};
use crate::storage::BlockStorageDiff;
use alloy_rlp_derive::{RlpDecodable, RlpEncodable};

/// Raw yield fields of a blast-geth account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlastAccountExt {
    /// Yield mode: 0 = automatic, 1 = disabled, 2 = claimable. Passed through
    /// unvalidated, as blast-geth does.
    pub flags: u8,
    pub fixed: U256,
    pub shares: U256,
    pub remainder: U256,
}

/// Wire form of a Blast account. Field order is the RLP order and must match
/// pipeline `types.BlastNewAccount`.
#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct BlastNewAccount {
    /// keccak256 of the account address.
    pub address: H256,
    pub nonce: u64,
    pub flags: u8,
    pub fixed: U256,
    pub shares: U256,
    pub remainder: U256,
    pub code_hash: H256,
}

impl BlastNewAccount {
    pub fn ext(&self) -> BlastAccountExt {
        BlastAccountExt {
            flags: self.flags,
            fixed: self.fixed,
            shares: self.shares,
            remainder: self.remainder,
        }
    }
}

pub type BlastBlockStorageDiff = BlockStorageDiff<BlastNewAccount>;

/// On-disk value of a Blast account; the address lives in the key.
#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub(crate) struct BlastSlimAccount {
    pub nonce: u64,
    pub flags: u8,
    pub fixed: U256,
    pub shares: U256,
    pub remainder: U256,
    pub code_hash: H256,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccountStorageDiff, IndexValuePair, NewCode};
    use alloy_rlp::{Decodable, Encodable};

    fn h(n: u8) -> H256 {
        H256::with_last_byte(n)
    }

    /// The same value pipeline `TestBlastBlockStorageDiffRLP` encodes.
    fn golden_value() -> BlastBlockStorageDiff {
        BlockStorageDiff {
            hash: h(1),
            parent_hash: h(2),
            new_accounts: vec![BlastNewAccount {
                address: h(3),
                nonce: 7,
                flags: 2,
                fixed: U256::from(11),
                shares: U256::from(13),
                remainder: U256::from(17),
                code_hash: h(4),
            }],
            deleted_accounts: vec![h(5)],
            storage_diffs: vec![AccountStorageDiff {
                address: h(6),
                diffs: vec![IndexValuePair {
                    index: h(7),
                    value: U256::from(19),
                }],
            }],
            new_codes: vec![NewCode {
                code_hash: h(8),
                code: vec![0xde, 0xad, 0xbe, 0xef].into(),
            }],
        }
    }

    #[test]
    fn blast_state_diff_matches_pipeline_golden_bytes() {
        let golden = crate::primitives::hex::decode(
            include_str!("../testdata/blast_state_diff.rlp.hex").trim(),
        )
        .unwrap();

        let mut encoded = Vec::new();
        golden_value().encode(&mut encoded);
        assert_eq!(encoded, golden);

        let decoded = BlastBlockStorageDiff::decode(&mut golden.as_slice()).unwrap();
        assert_eq!(decoded, golden_value());
    }
}
