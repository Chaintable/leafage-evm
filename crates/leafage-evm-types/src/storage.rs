use crate::primitives::{AccountInfo, BlockEnv, Bytes, H160, H256, U256};
use ethers_core::types::{Block, Transaction};
use ethers_core::utils::keccak256;
use open_fastrlp_derive::{RlpDecodable, RlpEncodable};
use revm::primitives::U256 as RU256;

pub fn block_env_from_block(block: &Block<Transaction>) -> BlockEnv {
    BlockEnv {
        number: RU256::from(block.number.unwrap_or_default().as_u64()),
        coinbase: block.author.unwrap_or_default().into(),
        timestamp: block.timestamp.into(),
        difficulty: block.difficulty.into(),
        basefee: block.base_fee_per_gas.unwrap_or_default().into(),
        gas_limit: block.gas_limit.into(),
        prevrandao: if block.difficulty.is_zero() {
            Some(block.mix_hash.unwrap_or_default().into())
        } else {
            None
        },
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

impl Into<AccountInfo> for NewAccount {
    fn into(self) -> AccountInfo {
        AccountInfo {
            balance: self.balance.into(),
            nonce: self.nonce,
            code_hash: self.code_hash.into(),
            code: None,
        }
    }
}

impl From<(H160, AccountInfo)> for NewAccount {
    fn from((address, account_info): (H160, AccountInfo)) -> Self {
        Self {
            address: keccak256(address.as_bytes()).into(),
            balance: account_info.balance.into(),
            nonce: account_info.nonce,
            code_hash: account_info.code_hash.into(),
        }
    }
}

impl From<(H256, AccountInfo)> for NewAccount {
    fn from((address, account_info): (H256, AccountInfo)) -> Self {
        Self {
            address,
            balance: account_info.balance.into(),
            nonce: account_info.nonce,
            code_hash: account_info.code_hash.into(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use open_fastrlp::Encodable;

    #[test]
    fn test_slim_account() {
        let account = NewAccount {
            address: H256::zero(),
            balance: U256::from(100),
            nonce: 0,
            code_hash: H256::zero(),
        };
        let slim_account = SlimAccount::from(account.clone());
        let mut buf = Vec::new();
        slim_account.encode(&mut buf);
        dbg!(buf.len());
    }
}
