use crate::primitives::{AccountInfo, Address, BlockEnv, Bytes, H256, RU256, U256};
use alloy::primitives::keccak256;
use alloy::rpc::types::{Block, Transaction};
use alloy_rlp_derive::{RlpDecodable, RlpEncodable};
use revm::primitives::BlobExcessGasAndPrice;

pub fn block_env_from_block(block: &Block<Transaction>) -> BlockEnv {
    let block_env = BlockEnv {
        number: RU256::from(block.header.number),
        coinbase: block.header.miner,
        timestamp: RU256::from(block.header.timestamp),
        difficulty: RU256::from(block.header.difficulty),
        basefee: RU256::from(block.header.base_fee_per_gas.unwrap_or_default()),
        gas_limit: RU256::from(block.header.gas_limit),
        prevrandao: if block.header.difficulty.is_zero() {
            block.header.mix_hash
        } else {
            Some(H256::ZERO)
        },
        blob_excess_gas_and_price: Some(BlobExcessGasAndPrice::new(
            block.header.excess_blob_gas.unwrap_or_default() as u64,
        )),
    };
    block_env
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
            code_hash: self.code_hash.0.into(),
            code: None,
        }
    }
}

impl From<(Address, AccountInfo)> for NewAccount {
    fn from((address, account_info): (Address, AccountInfo)) -> Self {
        Self {
            address: keccak256::<&[u8; 20]>(address.as_ref()).into(),
            balance: account_info.balance.into(),
            nonce: account_info.nonce,
            code_hash: account_info.code_hash.0.into(),
        }
    }
}

impl From<(H256, AccountInfo)> for NewAccount {
    fn from((address, account_info): (H256, AccountInfo)) -> Self {
        Self {
            address,
            balance: account_info.balance.into(),
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
}
