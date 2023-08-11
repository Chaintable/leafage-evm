use alloy_rlp::{Buf, Decodable, Encodable};
pub use revm::primitives::{AccountInfo, BlockEnv, Bytecode, Bytes, U256};
use revm::primitives::{B160, B256};

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct H256(pub B256);

impl H256 {
    pub fn zero() -> Self {
        Self(B256::zero())
    }
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self(B256::from_slice(bytes))
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

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct H160(pub B160);

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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BlockId {
    /// A block hash and an optional bool that defines if it's canonical
    Hash(H256),
    /// A block number
    Number(u64),
    /// The latest block
    Latest,
}
