use crate::primitives::{AccountInfo, Address, BlockEnv, Bytes, H256, U256};
use crate::rpc::{Block, Header};
use alloy::primitives::keccak256;
pub use alloy::serde::WithOtherFields;
use alloy_rlp::Decodable;
use alloy_rlp_derive::{RlpDecodable, RlpEncodable};
use revm::context_interface::block::BlobExcessGasAndPrice;
use serde::{Deserialize, Serialize};

pub type BlockInfo = WithOtherFields<Block<H256>>;
pub type HeaderInfo = WithOtherFields<Header>;

pub fn block_env_from_block<T>(block: &Block<T>) -> BlockEnv {
    BlockEnv {
        number: U256::from(block.header.number),
        beneficiary: block.header.beneficiary,
        timestamp: U256::from(block.header.timestamp),
        difficulty: block.header.difficulty,
        basefee: block.header.base_fee_per_gas.unwrap_or_default(),
        gas_limit: block.header.gas_limit,
        prevrandao: if block.header.difficulty.is_zero() {
            Some(block.header.mix_hash)
        } else {
            Some(H256::ZERO)
        },
        blob_excess_gas_and_price: block.header.excess_blob_gas.or(Some(0)).map(|excess_gas| {
            let blob_gasprice =
                alloy::eips::eip7840::BlobParams::cancun().calc_blob_fee(excess_gas);
            BlobExcessGasAndPrice {
                excess_blob_gas: excess_gas,
                blob_gasprice,
            }
        }),
        slot_num: Default::default(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebankOutPut {
    pub header: HeaderInfo,
    /// RLP encoded BlockStorageDiff
    pub state_diff: Bytes,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable, Default)]
pub struct BlockStorageDiff {
    /// Block root hash.
    pub hash: H256,
    /// Parent block root hash.
    pub parent_hash: H256,
    /// New accounts
    pub new_accounts: Vec<NewAccount>,
    /// Deleted accounts
    pub deleted_accounts: Vec<H256>,
    /// Account storage diff
    pub storage_diffs: Vec<AccountStorageDiff>,
    /// New codes
    pub new_codes: Vec<NewCode>,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct NewCode {
    pub code_hash: H256,
    pub code: Bytes,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct NewAccount {
    /// Account address
    pub address: H256,
    /// Account balance
    pub balance: U256,
    /// Account nonce
    pub nonce: u64,
    /// code hash
    pub code_hash: H256,
}

impl From<NewAccount> for AccountInfo {
    fn from(val: NewAccount) -> Self {
        AccountInfo {
            balance: val.balance,
            nonce: val.nonce,
            code_hash: val.code_hash.0.into(),
            code: None,
            account_id: Default::default(),
        }
    }
}

impl From<(Address, AccountInfo)> for NewAccount {
    fn from((address, account_info): (Address, AccountInfo)) -> Self {
        Self {
            address: keccak256::<&[u8; 20]>(address.as_ref()),
            balance: account_info.balance,
            nonce: account_info.nonce,
            code_hash: account_info.code_hash.0.into(),
        }
    }
}

impl From<(H256, AccountInfo)> for NewAccount {
    fn from((address, account_info): (H256, AccountInfo)) -> Self {
        Self {
            address,
            balance: account_info.balance,
            nonce: account_info.nonce,
            code_hash: account_info.code_hash.0.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct AccountStorageDiff {
    pub address: H256,
    pub diffs: Vec<IndexValuePair>,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct IndexValuePair {
    pub index: H256,
    pub value: U256,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct SlimAccount {
    /// Account balance
    pub balance: U256,
    /// Account nonce
    pub nonce: u64,
    /// code hash
    pub code_hash: H256,
}

impl From<NewAccount> for SlimAccount {
    fn from(account: NewAccount) -> Self {
        SlimAccount {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: account.code_hash,
        }
    }
}

/// Blast on-disk account value (`BlastSlimAccountV1`): raw yield fields
/// stored in place of a materialized balance. 6 RLP items, so a standard
/// 3-item [`SlimAccount`] can never decode as one (and vice versa).
#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct BlastSlimAccount {
    /// Account nonce
    pub nonce: u64,
    /// Yield mode: 0 = Automatic, 1 = Disabled, 2 = Claimable
    pub flags: u8,
    /// Balance for non-Automatic accounts
    pub fixed: U256,
    /// Yield shares for Automatic accounts
    pub shares: U256,
    /// Sub-share remainder for Automatic accounts
    pub remainder: U256,
    /// code hash
    pub code_hash: H256,
}

/// Blast (chain 81457) wire account. blast-geth stores raw yield fields
/// instead of a balance; the balance is derived at read time against the
/// sharePrice of the same state view: flags 0 (Automatic) ->
/// `shares * sharePrice + remainder`, any other flags -> `fixed`.
/// Field order mirrors Chaintable/pipeline `types.BlastNewAccount` exactly.
#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct BlastNewAccount {
    /// keccak256 of the account address
    pub address: H256,
    /// Account nonce
    pub nonce: u64,
    /// Yield mode: 0 = Automatic, 1 = Disabled, 2 = Claimable. Passed through
    /// as-is; unknown values are not rejected here (read side treats them
    /// like blast-geth `Balance()`: anything non-zero returns `fixed`).
    pub flags: u8,
    /// Balance for non-Automatic accounts
    pub fixed: U256,
    /// Yield shares for Automatic accounts
    pub shares: U256,
    /// Sub-share remainder for Automatic accounts
    pub remainder: U256,
    /// code hash
    pub code_hash: H256,
}

/// Blast wire state diff. Same layout as [`BlockStorageDiff`] except accounts
/// are [`BlastNewAccount`] (7 RLP items instead of 4). Mirrors
/// Chaintable/pipeline `types.BlastBlockStorageDiff` exactly.
#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable, Default)]
pub struct BlastBlockStorageDiff {
    /// Block root hash.
    pub hash: H256,
    /// Parent block root hash.
    pub parent_hash: H256,
    /// New accounts
    pub new_accounts: Vec<BlastNewAccount>,
    /// Deleted accounts
    pub deleted_accounts: Vec<H256>,
    /// Account storage diff
    pub storage_diffs: Vec<AccountStorageDiff>,
    /// New codes
    pub new_codes: Vec<NewCode>,
}

/// How an account's balance is represented internally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BalanceState {
    /// Materialized balance (all standard chains).
    Standard { balance: U256 },
    /// Blast raw yield fields; the balance is derived at read time against
    /// the sharePrice of the same state view.
    Blast {
        flags: u8,
        fixed: U256,
        shares: U256,
        remainder: U256,
    },
}

/// Chain-agnostic internal account value. Wire account types only exist at
/// decode boundaries and convert into this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccount {
    pub nonce: u64,
    pub code_hash: H256,
    pub balance_state: BalanceState,
}

impl StoredAccount {
    /// Materialized balance of a standard account. `None` for Blast accounts,
    /// whose balance is derived at read time against the sharePrice.
    pub fn standard_balance(&self) -> Option<U256> {
        match self.balance_state {
            BalanceState::Standard { balance } => Some(balance),
            BalanceState::Blast { .. } => None,
        }
    }
}

impl From<NewAccount> for StoredAccount {
    fn from(account: NewAccount) -> Self {
        StoredAccount {
            nonce: account.nonce,
            code_hash: account.code_hash,
            balance_state: BalanceState::Standard {
                balance: account.balance,
            },
        }
    }
}

impl From<BlastNewAccount> for StoredAccount {
    fn from(account: BlastNewAccount) -> Self {
        StoredAccount {
            nonce: account.nonce,
            code_hash: account.code_hash,
            balance_state: BalanceState::Blast {
                flags: account.flags,
                fixed: account.fixed,
                shares: account.shares,
                remainder: account.remainder,
            },
        }
    }
}

/// Internal account entry of a [`BlockStateUpdate`].
#[derive(Debug, Clone, PartialEq)]
pub struct AccountUpdate {
    /// keccak256 of the account address
    pub address: H256,
    pub account: StoredAccount,
}

/// Internal, non-RLP representation of one block's state changes. Wire types
/// ([`BlockStorageDiff`], [`BlastBlockStorageDiff`]) convert into this right
/// after decoding; everything past the decode boundary uses this type.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BlockStateUpdate {
    /// Block root hash.
    pub hash: H256,
    /// Parent block root hash.
    pub parent_hash: H256,
    /// New accounts
    pub new_accounts: Vec<AccountUpdate>,
    /// Deleted accounts
    pub deleted_accounts: Vec<H256>,
    /// Account storage diff
    pub storage_diffs: Vec<AccountStorageDiff>,
    /// New codes
    pub new_codes: Vec<NewCode>,
}

impl From<BlockStorageDiff> for BlockStateUpdate {
    fn from(diff: BlockStorageDiff) -> Self {
        BlockStateUpdate {
            hash: diff.hash,
            parent_hash: diff.parent_hash,
            new_accounts: diff
                .new_accounts
                .into_iter()
                .map(|account| AccountUpdate {
                    address: account.address,
                    account: account.into(),
                })
                .collect(),
            deleted_accounts: diff.deleted_accounts,
            storage_diffs: diff.storage_diffs,
            new_codes: diff.new_codes,
        }
    }
}

impl From<BlastBlockStorageDiff> for BlockStateUpdate {
    fn from(diff: BlastBlockStorageDiff) -> Self {
        BlockStateUpdate {
            hash: diff.hash,
            parent_hash: diff.parent_hash,
            new_accounts: diff
                .new_accounts
                .into_iter()
                .map(|account| AccountUpdate {
                    address: account.address,
                    account: account.into(),
                })
                .collect(),
            deleted_accounts: diff.deleted_accounts,
            storage_diffs: diff.storage_diffs,
            new_codes: diff.new_codes,
        }
    }
}

/// Which wire format a deployment's state diffs use. Always explicit
/// (configuration-driven) — never guessed by trying both formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateDiffCodec {
    Standard,
    BlastV1,
}

/// The single state-diff decode entry point. Empty bytes mean "no state
/// change in this block" and decode to an empty [`BlockStateUpdate`].
pub fn decode_state_diff(
    codec: StateDiffCodec,
    bytes: &[u8],
) -> Result<BlockStateUpdate, alloy_rlp::Error> {
    if bytes.is_empty() {
        return Ok(BlockStateUpdate::default());
    }
    match codec {
        StateDiffCodec::Standard => BlockStorageDiff::decode(&mut &*bytes).map(Into::into),
        StateDiffCodec::BlastV1 => BlastBlockStorageDiff::decode(&mut &*bytes).map(Into::into),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rlp::Encodable;
    #[test]
    fn test_slim_account() {
        let account = NewAccount {
            address: H256::default(),
            balance: U256::from(100),
            nonce: 0,
            code_hash: H256::default(),
        };
        let slim_account = SlimAccount::from(account.clone());
        let mut buf = Vec::new();
        slim_account.encode(&mut buf);
        dbg!(buf.len());
    }

    #[test]
    fn test_debank_output_deserialize() {
        let json = r#"{"header":{"number":"0x2","hash":"0x0dbde0ab2bd706dc3f4a90d67c9c50e77ffe16cc5d4cae1eea88c64a793a6054","parentHash":"0x0d68991dc0f42c522166e243f0d64a0f6ccf374dde2f4b30b37c131d73908989","nonce":"0x0000000000000000","mixHash":"0x0000000000000000000000000000000000000000000000000000000000000000","sha3Uncles":"0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347","logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000","stateRoot":"0x02f5551b11102f3f654e411c4bf5ff853364c7de69a3ea81b9b9e306a77a0dc0","miner":"0x0000000000000000000000000000000000000000","difficulty":"0x0","extraData":"0x","gasLimit":"0x1c9c380","gasUsed":"0x0","timestamp":"0x6715ede6","transactionsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","receiptsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421","baseFeePerGas":"0xf4240"},"state_diff":"0x"}"#;
        let output: DebankOutPut = serde_json::from_str(json).unwrap();
        assert_eq!(output.header.number, 2);
        assert_eq!(
            output.header.hash,
            "0x0dbde0ab2bd706dc3f4a90d67c9c50e77ffe16cc5d4cae1eea88c64a793a6054"
                .parse::<H256>()
                .unwrap()
        );
        assert_eq!(
            output.header.parent_hash,
            "0x0d68991dc0f42c522166e243f0d64a0f6ccf374dde2f4b30b37c131d73908989"
                .parse::<H256>()
                .unwrap()
        );
        assert_eq!(output.header.gas_limit, 0x1c9c380);
        assert_eq!(output.header.gas_used, 0);
        assert_eq!(output.header.timestamp, 0x6715ede6);
        assert_eq!(output.header.base_fee_per_gas, Some(0xf4240));
        assert!(output.state_diff.is_empty());
    }

    fn h256(n: u8) -> H256 {
        let mut bytes = [0u8; 32];
        bytes[31] = n;
        H256::from(bytes)
    }

    /// Locks the standard on-disk account encoding byte-for-byte. Existing
    /// standard-chain DBs must keep decoding; any change to these bytes is a
    /// breaking format change.
    #[test]
    fn test_slim_account_golden_bytes() {
        use crate::primitives::KECCAK256_EMPTY;

        let account = SlimAccount {
            balance: U256::from(100),
            nonce: 0,
            code_hash: H256::ZERO,
        };
        let mut buf = Vec::new();
        account.encode(&mut buf);
        let mut want = alloy::primitives::hex::decode("e36480a0").unwrap();
        want.extend_from_slice(&[0u8; 32]);
        assert_eq!(buf, want);
        assert_eq!(SlimAccount::decode(&mut buf.as_slice()).unwrap(), account);

        let account = SlimAccount {
            balance: U256::from(1_000_000_000_000_000_000u64),
            nonce: 5,
            code_hash: KECCAK256_EMPTY.0.into(),
        };
        let mut buf = Vec::new();
        account.encode(&mut buf);
        let want = alloy::primitives::hex::decode(
            "eb880de0b6b3a764000005a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
        )
        .unwrap();
        assert_eq!(buf, want);
        assert_eq!(SlimAccount::decode(&mut buf.as_slice()).unwrap(), account);
    }

    fn blast_diff_fixture_value() -> BlastBlockStorageDiff {
        BlastBlockStorageDiff {
            hash: h256(1),
            parent_hash: h256(2),
            new_accounts: vec![BlastNewAccount {
                address: h256(3),
                nonce: 7,
                flags: 2,
                fixed: U256::from(11),
                shares: U256::from(13),
                remainder: U256::from(17),
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
        assert!(BlockStorageDiff::decode(&mut blast_bytes.as_slice()).is_err());

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
    fn test_decode_state_diff() {
        // Empty bytes mean "no state change" under both codecs.
        assert_eq!(
            decode_state_diff(StateDiffCodec::Standard, &[]).unwrap(),
            BlockStateUpdate::default()
        );
        assert_eq!(
            decode_state_diff(StateDiffCodec::BlastV1, &[]).unwrap(),
            BlockStateUpdate::default()
        );

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

        let update = decode_state_diff(StateDiffCodec::Standard, &standard_bytes).unwrap();
        assert_eq!(update.hash, h256(1));
        assert_eq!(update.new_accounts.len(), 1);
        assert_eq!(update.new_accounts[0].address, h256(3));
        assert_eq!(
            update.new_accounts[0].account,
            StoredAccount {
                nonce: 7,
                code_hash: h256(4),
                balance_state: BalanceState::Standard {
                    balance: U256::from(100)
                },
            }
        );
        assert!(decode_state_diff(StateDiffCodec::BlastV1, &standard_bytes).is_err());

        let mut blast_bytes = Vec::new();
        blast_diff_fixture_value().encode(&mut blast_bytes);

        let update = decode_state_diff(StateDiffCodec::BlastV1, &blast_bytes).unwrap();
        assert_eq!(update.hash, h256(1));
        assert_eq!(update.deleted_accounts, vec![h256(5)]);
        assert_eq!(update.storage_diffs.len(), 1);
        assert_eq!(update.new_codes.len(), 1);
        assert_eq!(
            update.new_accounts[0].account,
            StoredAccount {
                nonce: 7,
                code_hash: h256(4),
                balance_state: BalanceState::Blast {
                    flags: 2,
                    fixed: U256::from(11),
                    shares: U256::from(13),
                    remainder: U256::from(17),
                },
            }
        );
        assert!(decode_state_diff(StateDiffCodec::Standard, &blast_bytes).is_err());
    }
}
