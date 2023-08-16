use std::str::FromStr;

use alloy_rlp::{Buf, Decodable, Encodable};
pub use revm::primitives::{AccountInfo, BlockEnv, Bytecode, Bytes, U256};
use revm::primitives::{B160, B256};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash, Deserialize, Serialize)]
pub struct H256(pub B256);

impl H256 {
    pub fn zero() -> Self {
        Self(B256::zero())
    }

    pub fn from_slice(bytes: &[u8]) -> Self {
        if bytes.len() < 32 {
            let mut b = [0u8; 32];
            b[32 - bytes.len()..].copy_from_slice(bytes);
            return Self(B256::from_slice(&b));
        }
        Self(B256::from_slice(bytes))
    }

    pub fn trim_left_zero(&self) -> Vec<u8> {
        let bytes = self.0.as_bytes();
        let mut i = 0;
        while i < bytes.len() && bytes[i] == 0 {
            i += 1;
        }
        bytes[i..].to_vec()
    }
}

impl Default for H256 {
    fn default() -> Self {
        Self(B256::default())
    }
}

impl AsRef<B256> for H256 {
    fn as_ref(&self) -> &B256 {
        &self.0
    }
}

impl From<B256> for H256 {
    fn from(b: B256) -> Self {
        Self(b)
    }
}

impl Into<B256> for H256 {
    fn into(self) -> B256 {
        self.0
    }
}

impl FromStr for H256 {
    type Err = rustc_hex::FromHexError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        B256::from_str(s).map(|h| h.into())
    }
}

impl Encodable for H256 {
    fn length(&self) -> usize {
        B256::len_bytes()
    }
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        out.put_slice(self.0.as_bytes());
    }
}

impl Decodable for H256 {
    fn decode(rlp: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
        let mut bytes = [0u8; 32];
        rlp.copy_to_slice(&mut bytes);
        Ok(Self(bytes.into()))
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash, Deserialize, Serialize)]
pub struct H160(pub B160);

impl H160 {
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self(B160::from_slice(bytes))
    }
}

impl Default for H160 {
    fn default() -> Self {
        Self(B160::default())
    }
}

impl AsRef<B160> for H160 {
    fn as_ref(&self) -> &B160 {
        &self.0
    }
}

impl From<B160> for H160 {
    fn from(b: B160) -> Self {
        Self(b)
    }
}

impl Into<B160> for H160 {
    fn into(self) -> B160 {
        self.0
    }
}

impl Encodable for H160 {
    fn length(&self) -> usize {
        B160::len_bytes()
    }
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        out.put_slice(self.0.as_bytes());
    }
}

impl Decodable for H160 {
    fn decode(rlp: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
        let mut bytes = [0u8; 20];
        rlp.copy_to_slice(&mut bytes);
        Ok(Self(bytes.into()))
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccessListItem {
    pub address: H160,
    pub storage_keys: Vec<H256>,
}

pub type AccessList = Vec<AccessListItem>;

pub fn access_list_flattened(access_list: AccessList) -> Vec<(B160, Vec<U256>)> {
    access_list
        .into_iter()
        .map(|item| {
            (
                item.address.into(),
                item.storage_keys
                    .into_iter()
                    .map(|v| {
                        let v: B256 = v.into();
                        U256::from_be_bytes(v.0)
                    })
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
