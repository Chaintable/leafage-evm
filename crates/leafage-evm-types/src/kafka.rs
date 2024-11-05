use serde::{Deserialize, Serialize};

use crate::primitives::H256;

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
