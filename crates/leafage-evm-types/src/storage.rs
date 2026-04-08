use crate::primitives::{AccountInfo, Address, BlockEnv, Bytes, H256, U256};
use crate::rpc::{Block, Header};
use alloy::primitives::keccak256;
pub use alloy::serde::WithOtherFields;
use alloy_rlp_derive::{RlpDecodable, RlpEncodable};
use revm::context_interface::block::BlobExcessGasAndPrice;
use serde::{Deserialize, Serialize};

pub type BlockInfo = WithOtherFields<Block<H256>>;

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
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebankOutPut {
    pub header: Header,
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
}
