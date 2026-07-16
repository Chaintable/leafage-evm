use crate::blast::{BlastAccountExt, BlastBlockStorageDiff, BlastSlimAccountV1};
use crate::primitives::{AccountInfo, Address, BlockEnv, Bytes, H256, KECCAK256_EMPTY, U256};
use crate::rpc::{Block, Header};
use alloy::primitives::keccak256;
pub use alloy::serde::WithOtherFields;
use alloy_rlp::{Decodable, Encodable};
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

/// Wire state diff of one block. The container is generic over the account
/// entry type: standard chains use the default [`NewAccount`] (4 RLP items
/// per account); chains with a rewritten account model plug in their own
/// wire account (e.g. [`BlastNewAccount`](crate::blast::BlastNewAccount),
/// 7 items). The 6-item top-level layout and the standard-chain bytes are
/// unchanged: `BlockStorageDiff` still means `BlockStorageDiff<NewAccount>`.
///
/// The internal, non-RLP representation of the same container is
/// [`BlockStateUpdate`].
#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct BlockStorageDiff<A = NewAccount> {
    /// Block root hash.
    pub hash: H256,
    /// Parent block root hash.
    pub parent_hash: H256,
    /// New accounts
    pub new_accounts: Vec<A>,
    /// Deleted accounts
    pub deleted_accounts: Vec<H256>,
    /// Account storage diff
    pub storage_diffs: Vec<AccountStorageDiff>,
    /// New codes
    pub new_codes: Vec<NewCode>,
}

// Manual impl instead of derive: the derived one would demand `A: Default`,
// which the account entry types neither have nor need for an empty diff.
impl<A> Default for BlockStorageDiff<A> {
    fn default() -> Self {
        Self {
            hash: H256::default(),
            parent_hash: H256::default(),
            new_accounts: Vec::new(),
            deleted_accounts: Vec::new(),
            storage_diffs: Vec::new(),
            new_codes: Vec::new(),
        }
    }
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

/// Chain-agnostic internal account value. Wire account types only exist at
/// decode boundaries and convert into this.
///
/// A generic core (same shape as [`NewAccount`]: balance, nonce, code_hash)
/// with an embedded chain-specific extension: standard chains carry no
/// extension, chains with a rewritten account model (Blast) embed their raw
/// fields in `ext` and the balance is derived at read time.
///
/// All fields are private on purpose: the invariant "`ext` is `Some` ⇒
/// `balance` is zero and must not be read" is established once by the
/// constructors and upheld by the accessor API ([`Self::standard_balance`],
/// [`Self::balance_view`]), instead of relying on every consumer to know the
/// rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAccount {
    /// Materialized balance; only meaningful while `ext` is `None`.
    balance: U256,
    nonce: u64,
    code_hash: H256,
    /// Chain-specific account-model extension. `None` = standard account.
    ext: Option<AccountExt>,
}

/// Chain-specific account-model extensions; new chains add a variant here.
/// Deliberately NOT `#[non_exhaustive]`: adding a variant must force every
/// match site to decide explicitly, never fall through a default arm that
/// silently treats an unknown extension as a standard account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountExt {
    Blast(BlastAccountExt),
}

/// Borrowed balance view of a [`StoredAccount`], see
/// [`StoredAccount::balance_view`]. A read API only — the storage layout
/// stays the embedded struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BalanceView<'a> {
    /// Materialized balance of a standard account.
    Standard(U256),
    /// Blast raw yield fields; the balance must be derived against the
    /// sharePrice of the same state view.
    Blast(&'a BlastAccountExt),
}

impl StoredAccount {
    /// Standard account (materialized balance, no extension).
    pub fn standard(balance: U256, nonce: u64, code_hash: H256) -> Self {
        Self {
            balance,
            nonce,
            code_hash,
            ext: None,
        }
    }

    /// Account with a chain-specific extension. The materialized balance is
    /// pinned to zero here — the extension is the balance's source of truth.
    pub fn with_ext(nonce: u64, code_hash: H256, ext: AccountExt) -> Self {
        Self {
            balance: U256::ZERO,
            nonce,
            code_hash,
            ext: Some(ext),
        }
    }

    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    pub fn code_hash(&self) -> H256 {
        self.code_hash
    }

    pub fn ext(&self) -> Option<&AccountExt> {
        self.ext.as_ref()
    }

    /// Materialized balance of a standard account. `None` for extended
    /// accounts, whose balance must be derived at read time.
    pub fn standard_balance(&self) -> Option<U256> {
        self.ext.is_none().then_some(self.balance)
    }

    /// The authoritative balance view: consumers match on this, so reading
    /// "the materialized balance of an extended account" is structurally
    /// impossible.
    pub fn balance_view(&self) -> BalanceView<'_> {
        match &self.ext {
            None => BalanceView::Standard(self.balance),
            Some(AccountExt::Blast(blast)) => BalanceView::Blast(blast),
        }
    }
}

impl From<NewAccount> for StoredAccount {
    fn from(account: NewAccount) -> Self {
        StoredAccount::standard(account.balance, account.nonce, account.code_hash)
    }
}

/// Internal account entry of a [`BlockStateUpdate`].
#[derive(Debug, Clone, PartialEq)]
pub struct AccountUpdate {
    /// keccak256 of the account address
    pub address: H256,
    pub account: StoredAccount,
}

impl From<NewAccount> for AccountUpdate {
    fn from(account: NewAccount) -> Self {
        AccountUpdate {
            address: account.address,
            account: account.into(),
        }
    }
}

/// Internal, non-RLP representation of one block's state changes: the same
/// generic container instantiated with the internal account entry. Wire
/// instantiations convert into it right after decoding; everything past the
/// decode boundary uses this type. [`AccountUpdate`] implements no RLP
/// traits, so this instantiation structurally has no wire codec.
pub type BlockStateUpdate = BlockStorageDiff<AccountUpdate>;

impl<A> BlockStorageDiff<A> {
    /// Convert a decoded wire diff into the internal representation.
    ///
    /// An inherent method rather than `From`: with [`BlockStateUpdate`] being
    /// an instantiation of this same container, a generic `From` impl would
    /// overlap the reflexive `impl From<T> for T` at `A = AccountUpdate`.
    pub fn into_state_update(self) -> BlockStateUpdate
    where
        AccountUpdate: From<A>,
    {
        BlockStateUpdate {
            hash: self.hash,
            parent_hash: self.parent_hash,
            new_accounts: self.new_accounts.into_iter().map(Into::into).collect(),
            deleted_accounts: self.deleted_accounts,
            storage_diffs: self.storage_diffs,
            new_codes: self.new_codes,
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
        StateDiffCodec::Standard => BlockStorageDiff::<NewAccount>::decode(&mut &*bytes)
            .map(BlockStorageDiff::into_state_update),
        StateDiffCodec::BlastV1 => {
            BlastBlockStorageDiff::decode(&mut &*bytes).map(BlockStorageDiff::into_state_update)
        }
    }
}

/// Encode the on-disk account value. Standard accounts keep the exact
/// 3-item [`SlimAccount`] bytes; extended accounts encode their chain shape
/// ([`BlastSlimAccountV1`] for Blast) without any balance — the private
/// fields plus the view API make "encoding a balance for an extended
/// account" structurally impossible. `code_hash` is written verbatim (no
/// normalization), mirroring the existing encode side.
pub fn encode_stored_account(account: &StoredAccount) -> Vec<u8> {
    let mut buf = Vec::new();
    match account.balance_view() {
        BalanceView::Standard(balance) => SlimAccount {
            balance,
            nonce: account.nonce(),
            code_hash: account.code_hash(),
        }
        .encode(&mut buf),
        BalanceView::Blast(ext) => BlastSlimAccountV1 {
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

/// Decode the on-disk account value strictly by codec: a `Standard`
/// deployment only decodes the 3-item [`SlimAccount`], a `BlastV1`
/// deployment only the 6-item [`BlastSlimAccountV1`]. A shape mismatch is
/// an RLP error — fail closed, never guess the format.
///
/// Empty bytes are NOT this function's business: the archive deletion
/// sentinel (empty value) must be ruled out by the caller with
/// `is_empty()` before calling.
pub fn decode_stored_account(
    bytes: &[u8],
    codec: StateDiffCodec,
) -> Result<StoredAccount, alloy_rlp::Error> {
    match codec {
        StateDiffCodec::Standard => {
            let account = SlimAccount::decode(&mut &*bytes)?;
            Ok(StoredAccount::standard(
                account.balance,
                account.nonce,
                normalize_code_hash(account.code_hash),
            ))
        }
        StateDiffCodec::BlastV1 => {
            let account = BlastSlimAccountV1::decode(&mut &*bytes)?;
            Ok(StoredAccount::with_ext(
                account.nonce,
                normalize_code_hash(account.code_hash),
                AccountExt::Blast(BlastAccountExt {
                    flags: account.flags,
                    fixed: account.fixed,
                    shares: account.shares,
                    remainder: account.remainder,
                }),
            ))
        }
    }
}

/// On-disk a code-less account stores a zero `code_hash`; in memory it is
/// `KECCAK256_EMPTY`. Converges the normalization previously inlined at
/// every backend decode site.
fn normalize_code_hash(code_hash: H256) -> H256 {
    if code_hash.is_zero() {
        KECCAK256_EMPTY
    } else {
        code_hash
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
        let account = SlimAccount {
            balance: U256::from(100),
            nonce: 0,
            code_hash: H256::ZERO,
        };
        let mut buf = Vec::new();
        account.encode(&mut buf);
        let want = crate::primitives::hex::decode(concat!(
            "e36480a0",
            "0000000000000000000000000000000000000000000000000000000000000000"
        ))
        .unwrap();
        assert_eq!(buf, want);
        assert_eq!(SlimAccount::decode(&mut buf.as_slice()).unwrap(), account);

        let account = SlimAccount {
            balance: U256::from(1_000_000_000_000_000_000u64),
            nonce: 5,
            code_hash: crate::primitives::KECCAK256_EMPTY,
        };
        let mut buf = Vec::new();
        account.encode(&mut buf);
        let want = crate::primitives::hex::decode(
            "eb880de0b6b3a764000005a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
        )
        .unwrap();
        assert_eq!(buf, want);
        assert_eq!(SlimAccount::decode(&mut buf.as_slice()).unwrap(), account);
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

        let diff = BlockStorageDiff {
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
        let mut bytes = Vec::new();
        diff.encode(&mut bytes);

        let update = decode_state_diff(StateDiffCodec::Standard, &bytes).unwrap();
        assert_eq!(update.hash, h256(1));
        assert_eq!(update.parent_hash, h256(2));
        assert_eq!(
            update.new_accounts,
            vec![AccountUpdate {
                address: h256(3),
                account: StoredAccount::standard(U256::from(100), 7, h256(4)),
            }]
        );
        // A standard account exposes its materialized balance.
        let account = &update.new_accounts[0].account;
        assert_eq!(account.standard_balance(), Some(U256::from(100)));
        assert_eq!(
            account.balance_view(),
            BalanceView::Standard(U256::from(100))
        );

        // A diff carrying a 4-field standard account is not Blast-decodable.
        assert!(decode_state_diff(StateDiffCodec::BlastV1, &bytes).is_err());
    }

    /// The Standard arm of `encode_stored_account` must produce the exact
    /// bytes locked by `test_slim_account_golden_bytes`.
    #[test]
    fn test_encode_stored_account_standard_golden() {
        let account = StoredAccount::standard(U256::from(100), 0, H256::ZERO);
        let want = crate::primitives::hex::decode(concat!(
            "e36480a0",
            "0000000000000000000000000000000000000000000000000000000000000000"
        ))
        .unwrap();
        assert_eq!(encode_stored_account(&account), want);

        let account =
            StoredAccount::standard(U256::from(1_000_000_000_000_000_000u64), 5, KECCAK256_EMPTY);
        let bytes = encode_stored_account(&account);
        let want = crate::primitives::hex::decode(
            "eb880de0b6b3a764000005a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
        )
        .unwrap();
        assert_eq!(bytes, want);
        assert_eq!(
            decode_stored_account(&bytes, StateDiffCodec::Standard).unwrap(),
            account
        );
    }

    /// Blast arm of `encode_stored_account` == `BlastSlimAccountV1` bytes,
    /// and the value round-trips through `decode_stored_account`.
    #[test]
    fn test_stored_account_blast_round_trip() {
        let account = StoredAccount::with_ext(
            7,
            h256(4),
            AccountExt::Blast(BlastAccountExt {
                flags: 2,
                fixed: U256::from(11),
                shares: U256::from(13),
                remainder: U256::from(17),
            }),
        );
        let bytes = encode_stored_account(&account);
        // Same 6-item bytes locked by blast::tests::test_blast_slim_account_v1_bytes.
        let want = crate::primitives::hex::decode(concat!(
            "e607020b0d11a0",
            "00000000000000000000000000000000000000000000000000000000000000",
            "04"
        ))
        .unwrap();
        assert_eq!(bytes, want);
        assert_eq!(
            decode_stored_account(&bytes, StateDiffCodec::BlastV1).unwrap(),
            account
        );
    }

    /// On-disk records are decoded strictly by codec: a 3-item record under
    /// BlastV1 and a 6-item record under Standard are both RLP errors.
    #[test]
    fn test_decode_stored_account_strict_shape() {
        let standard = StoredAccount::standard(U256::from(100), 7, h256(4));
        let standard_bytes = encode_stored_account(&standard);
        assert!(decode_stored_account(&standard_bytes, StateDiffCodec::BlastV1).is_err());

        let blast = StoredAccount::with_ext(
            7,
            h256(4),
            AccountExt::Blast(BlastAccountExt {
                flags: 2,
                fixed: U256::from(11),
                shares: U256::from(13),
                remainder: U256::from(17),
            }),
        );
        let blast_bytes = encode_stored_account(&blast);
        assert!(decode_stored_account(&blast_bytes, StateDiffCodec::Standard).is_err());
    }

    /// A zero `code_hash` on disk normalizes to `KECCAK256_EMPTY` — for both
    /// record shapes, matching the read-side normalization inlined at the
    /// backend decode sites on main. A non-zero `code_hash` passes through.
    #[test]
    fn test_decode_stored_account_code_hash_normalization() {
        // 3-item record.
        let account = StoredAccount::standard(U256::from(100), 7, H256::ZERO);
        let bytes = encode_stored_account(&account);
        let decoded = decode_stored_account(&bytes, StateDiffCodec::Standard).unwrap();
        assert_eq!(decoded.code_hash(), KECCAK256_EMPTY);
        assert_eq!(decoded.standard_balance(), Some(U256::from(100)));
        assert_eq!(decoded.nonce(), 7);

        // 6-item record.
        let account = StoredAccount::with_ext(
            7,
            H256::ZERO,
            AccountExt::Blast(BlastAccountExt {
                flags: 2,
                fixed: U256::from(11),
                shares: U256::from(13),
                remainder: U256::from(17),
            }),
        );
        let bytes = encode_stored_account(&account);
        let decoded = decode_stored_account(&bytes, StateDiffCodec::BlastV1).unwrap();
        assert_eq!(decoded.code_hash(), KECCAK256_EMPTY);
        assert_eq!(decoded.nonce(), 7);

        // Non-zero passes through untouched.
        let account = StoredAccount::standard(U256::from(100), 7, h256(9));
        let bytes = encode_stored_account(&account);
        let decoded = decode_stored_account(&bytes, StateDiffCodec::Standard).unwrap();
        assert_eq!(decoded.code_hash(), h256(9));
    }
}
