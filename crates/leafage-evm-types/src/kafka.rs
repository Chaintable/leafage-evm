use crate::primitives::H256;
use serde::{Deserialize, Serialize};
use std::io::Read;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KafkaBlockContext {
    pub hash: H256,
    pub parent_hash: H256,
    pub block_number: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KafkaBlockChangeNotification {
    pub change_type: u64,
    pub new_blocks: Vec<KafkaBlockContext>,
    pub drop_blocks: Vec<KafkaBlockContext>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Failed to parse KafkaBlockChangeNotification from bytes")]
    SerdeJson(#[from] serde_json::Error),
    #[error("Failed to read bytes")]
    Io(#[from] std::io::Error),
}

impl TryFrom<&[u8]> for KafkaBlockChangeNotification {
    type Error = Error;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        let mut gz = flate2::read::GzDecoder::new(&bytes[..]);
        let mut bytes = Vec::new();
        gz.read_to_end(&mut bytes)?;
        let notification: KafkaBlockChangeNotification = serde_json::from_slice(&bytes)?;
        Ok(notification)
    }
}
