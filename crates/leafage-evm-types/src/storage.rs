use crate::primitives::{AccountInfo, BlockEnv, Bytecode, Bytes, H160, H256, U256};
use alloy_rlp_derive::{RlpDecodable, RlpEncodable};

#[derive(Debug, PartialEq, RlpDecodable, RlpEncodable, Clone)]
pub struct BlockInfo {
    /// Block number.
    pub number: U256,
    /// Block hash.
    pub hash: H256,
    /// Parent block hash.
    pub parent_hash: H256,
    /// Coinbase or miner or address that created and signed the block.
    /// Address where we are going to send gas spend
    pub coinbase: H160,
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

impl Into<BlockEnv> for BlockInfo {
    fn into(self) -> BlockEnv {
        BlockEnv {
            number: self.number,
            coinbase: self.coinbase.into(),
            timestamp: self.timestamp,
            difficulty: self.difficulty,
            basefee: self.basefee,
            gas_limit: self.gas_limit,
            prevrandao: if self.prevrandao.as_ref().is_zero() {
                None
            } else {
                Some(self.prevrandao.into())
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct BlockStorageDiff {
    /// Block root hash.
    pub hash: H256,
    /// Parent block root hash.
    pub parent_hash: H256,
    /// New accounts
    pub new_accounts: Vec<NewAccount>,
    /// Deleted accounts
    pub deleted_accounts: Vec<H160>,
    /// Account storage diff
    pub storage_diff: Vec<AccountStorageDiff>,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct NewAccount {
    /// Account address
    pub address: H160,
    /// Account balance
    pub balance: U256,
    /// Account nonce
    pub nonce: u64,
    /// code hash
    pub code_hash: H256,
    /// code
    pub code: Bytes,
}

impl Into<AccountInfo> for NewAccount {
    fn into(self) -> AccountInfo {
        AccountInfo {
            balance: self.balance,
            nonce: self.nonce,
            code_hash: self.code_hash.into(),
            code: if self.code.is_empty() {
                None
            } else {
                unsafe {
                    Some(Bytecode::new_raw_with_hash(
                        self.code.into(),
                        self.code_hash.into(),
                    ))
                }
            },
        }
    }
}

impl From<(H160, AccountInfo)> for NewAccount {
    fn from((address, account_info): (H160, AccountInfo)) -> Self {
        Self {
            address: address.into(),
            balance: account_info.balance,
            nonce: account_info.nonce,
            code_hash: account_info.code_hash.into(),
            code: account_info
                .code
                .map(|code| code.original_bytes().into())
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct AccountStorageDiff {
    pub account_addr: H160,
    pub value: Vec<IndexValuePair>,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct IndexValuePair {
    pub index: U256,
    pub value: U256,
}
