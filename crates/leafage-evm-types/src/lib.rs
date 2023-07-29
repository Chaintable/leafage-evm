use reth_primitives::{Address, Bytes, H256, U256};
use reth_rlp_derive::{RlpDecodable, RlpEncodable};
use revm::primitives::{AccountInfo, BlockEnv, Bytecode};

#[derive(Debug, PartialEq, RlpDecodable, RlpEncodable, Clone)]
pub struct BlockInfo {
    /// Block number.
    pub number: U256,
    /// Block hash.
    pub hash: H256,
    /// Parent block hash.
    pub parent_hash: H256,
    /// Block root hash.
    pub root: H256,
    /// Parent block root hash.
    pub parent_root: H256,
    /// Coinbase or miner or address that created and signed the block.
    /// Address where we are going to send gas spend
    pub coinbase: Address,
    /// Timestamp of block.
    pub timestamp: U256,
    /// Difficulty is removed and not used after Paris (aka TheMerge). Value is replaced with prevrandao.
    pub difficulty: U256,
    /// Basefee is added in EIP1559 London upgrade
    pub basefee: U256,
    /// Gas limit of block.
    pub gas_limit: U256,
    /// Prevrandao is used after Paris (aka TheMerge) instead of the difficulty value.
    pub prevrandao: H256,
}

impl Into<BlockEnv> for &BlockInfo {
    fn into(self) -> BlockEnv {
        BlockEnv {
            number: self.number,
            coinbase: self.coinbase,
            timestamp: self.timestamp,
            difficulty: self.difficulty,
            basefee: self.basefee,
            gas_limit: self.gas_limit,
            prevrandao: if self.prevrandao.is_zero() {
                None
            } else {
                Some(self.prevrandao)
            },
        }
    }
}

#[derive(Debug, PartialEq, RlpDecodable, RlpEncodable)]
pub struct BlockDiff {
    /// Block root hash.
    pub root: H256,
    /// Parent block root hash.
    pub parent_root: H256,
    /// New accounts
    pub accounts_diff: Vec<RawAccountChange>,
    /// Account storage diff
    pub storage_diff: Vec<AccountDiff>,
}

#[derive(Debug, PartialEq, RlpDecodable, RlpEncodable)]
#[rlp(trailing)]
pub struct RawAccountChange {
    /// Account address
    pub address: Address,
    /// Account info
    pub info: Option<RawAccount>,
}

#[derive(Debug, PartialEq, RlpDecodable, RlpEncodable)]
pub struct RawAccount {
    /// Account balance
    pub balance: U256,
    /// Account nonce
    pub nonce: u64,
    /// code hash
    pub code_hash: H256,
    /// code
    pub code: Bytes,
}

impl Into<AccountInfo> for RawAccount {
    fn into(self) -> AccountInfo {
        AccountInfo {
            balance: self.balance,
            nonce: self.nonce,
            code_hash: self.code_hash,
            code: if self.code.is_empty() {
                None
            } else {
                unsafe {
                    Some(Bytecode::new_raw_with_hash(
                        self.code.into(),
                        self.code_hash,
                    ))
                }
            },
        }
    }
}

impl From<AccountInfo> for RawAccount {
    fn from(account_info: AccountInfo) -> Self {
        Self {
            balance: account_info.balance,
            nonce: account_info.nonce,
            code_hash: account_info.code_hash,
            code: account_info
                .code
                .map(|code| code.original_bytes().into())
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, PartialEq, RlpDecodable, RlpEncodable, Default)]
pub struct AccountDiff {
    pub account_addr: Address,
    pub value: Vec<IndexValuePair>,
}

#[derive(Debug, PartialEq, RlpDecodable, RlpEncodable)]
pub struct IndexValuePair {
    pub index: U256,
    pub value: U256,
}
