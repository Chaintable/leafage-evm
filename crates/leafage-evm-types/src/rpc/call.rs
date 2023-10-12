use crate::primitives::{AccessList, Bytes, H160, H256, U256};
use serde::{Deserialize, Serialize};

/// Call request
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CallRequest {
    /// From
    pub from: Option<H160>,
    /// To
    pub to: Option<H160>,
    /// Gas Price
    pub gas_price: Option<U256>,
    /// EIP-1559 Max base fee the caller is willing to pay
    pub max_fee_per_gas: Option<U256>,
    /// EIP-1559 Priority fee the caller is paying to the block author
    pub max_priority_fee_per_gas: Option<U256>,
    /// Gas
    pub gas: Option<U256>,
    /// Value
    pub value: Option<U256>,
    /// Transaction data
    ///
    /// This accepts both `input` and `data`
    #[serde(alias = "input")]
    pub data: Option<Bytes>,
    /// Nonce
    pub nonce: Option<U256>,
    /// chain id
    pub chain_id: Option<U256>,
    /// AccessList
    pub access_list: Option<AccessList>,
    /// Max Fee per Blob gas for EIP-4844 transactions
    pub max_fee_per_blob_gas: Option<U256>,
    /// Blob Versioned Hashes for EIP-4844 transactions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob_versioned_hashes: Option<Vec<H256>>,
}
