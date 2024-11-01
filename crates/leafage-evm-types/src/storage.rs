use crate::primitives::{AccountInfo, Address, BlockEnv, Bytes, H256, RU256, U256};
use crate::rpc::{Block, Transaction};
use alloy::primitives::keccak256;
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

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable)]
pub struct SlimBlockEnv {
    /// The number of ancestor blocks of this block (block height).
    pub number: U256,
    /// Coinbase or miner or address that created and signed the block.
    ///
    /// This is the receiver address of all the gas spent in the block.
    pub coinbase: Address,

    /// The timestamp of the block in seconds since the UNIX epoch.
    pub timestamp: U256,
    /// The gas limit of the block.
    pub gas_limit: U256,
    /// The base fee per gas, added in the London upgrade with [EIP-1559].
    ///
    /// [EIP-1559]: https://eips.ethereum.org/EIPS/eip-1559
    pub basefee: U256,
    /// The difficulty of the block.
    ///
    /// Unused after the Paris (AKA the merge) upgrade, and replaced by `prevrandao`.
    pub difficulty: U256,
    /// The output of the randomness beacon provided by the beacon chain.
    ///
    /// Replaces `difficulty` after the Paris (AKA the merge) upgrade with [EIP-4399].
    ///
    /// NOTE: `prevrandao` can be found in a block in place of `mix_hash`.
    ///
    /// [EIP-4399]: https://eips.ethereum.org/EIPS/eip-4399
    pub prevrandao: H256,
    /// Excess blob gas and blob gasprice.
    /// See also [`crate::calc_excess_blob_gas`]
    /// and [`calc_blob_gasprice`].
    ///
    /// Incorporated as part of the Cancun upgrade via [EIP-4844].
    ///
    /// [EIP-4844]: https://eips.ethereum.org/EIPS/eip-4844
    pub blob_excess_gas_and_price: SlimBlobExcessGasAndPrice,
}

#[derive(Debug, Clone, PartialEq, RlpDecodable, RlpEncodable, Default)]
pub struct SlimBlobExcessGasAndPrice {
    /// The excess blob gas of the block.
    pub excess_blob_gas: u64,
    /// The calculated blob gas price based on the `excess_blob_gas`, See [calc_blob_gasprice]
    pub blob_gasprice: u128,
}

impl Into<BlockEnv> for SlimBlockEnv {
    fn into(self) -> BlockEnv {
        BlockEnv {
            number: RU256::from(self.number),
            coinbase: self.coinbase,
            timestamp: RU256::from(self.timestamp),
            difficulty: RU256::from(self.difficulty),
            basefee: RU256::from(self.basefee),
            gas_limit: RU256::from(self.gas_limit),
            prevrandao: Some(self.prevrandao),
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice::new(
                self.blob_excess_gas_and_price.excess_blob_gas,
            )),
        }
    }
}

impl From<BlockEnv> for SlimBlockEnv {
    fn from(block_env: BlockEnv) -> Self {
        SlimBlockEnv {
            number: block_env.number,
            coinbase: block_env.coinbase,
            timestamp: block_env.timestamp,
            difficulty: block_env.difficulty,
            basefee: block_env.basefee,
            gas_limit: block_env.gas_limit,
            prevrandao: block_env.prevrandao.unwrap_or_default(),
            blob_excess_gas_and_price: block_env
                .blob_excess_gas_and_price
                .map(|blob_excess_gas_and_price| SlimBlobExcessGasAndPrice {
                    excess_blob_gas: blob_excess_gas_and_price.excess_blob_gas as u64,
                    blob_gasprice: blob_excess_gas_and_price.blob_gasprice,
                })
                .unwrap_or_default(),
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
