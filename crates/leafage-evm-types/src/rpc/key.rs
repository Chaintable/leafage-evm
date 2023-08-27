use crate::primitives::{trim_left_zero_bytes, H256, U256};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(from = "U256", into = "String")]
pub struct JsonStorageKey(pub H256);

impl From<U256> for JsonStorageKey {
    fn from(value: U256) -> Self {
        let bytes: [u8; 32] = value.into();
        // SAFETY: Address (H256) and U256 have the same number of bytes
        JsonStorageKey(H256::from(bytes))
    }
}

impl From<JsonStorageKey> for String {
    fn from(value: JsonStorageKey) -> Self {
        use std::fmt::Write;
        // SAFETY: Address (H256) and U256 have the same number of bytes
        let uint = U256::from_big_endian(value.0.as_bytes());
        let array: [u8; 32] = uint.into();
        let bytes = trim_left_zero_bytes(&array);
        let mut hex = String::with_capacity(2 + bytes.len() * 2);
        hex.push_str("0x");
        for byte in bytes {
            write!(hex, "{:02x}", byte).unwrap();
        }
        hex
    }
}
