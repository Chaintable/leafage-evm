pub use ethers_core::types::{Address, Bytes, H160, H256, U256};
use revm::primitives::B160;
pub use revm::primitives::{AccountInfo, BlockEnv, Bytecode, U256 as RU256};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccessListItem {
    pub address: Address,
    pub storage_keys: Vec<H256>,
}

pub type AccessList = Vec<AccessListItem>;

pub fn access_list_flattened(access_list: AccessList) -> Vec<(B160, Vec<RU256>)> {
    access_list
        .into_iter()
        .map(|item| {
            (
                item.address.into(),
                item.storage_keys
                    .into_iter()
                    .map(|v| RU256::from_be_bytes(v.into()))
                    .collect(),
            )
        })
        .collect()
}

pub fn trim_left_zero_bytes(bytes: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < bytes.len() && bytes[i] == 0 {
        i += 1;
    }
    &bytes[i..]
}
