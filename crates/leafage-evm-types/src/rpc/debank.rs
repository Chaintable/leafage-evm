use super::{
    super::primitives::{Address, Bytes, H256, U256},
    BlockId,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BlockContext {
    block_id: BlockId,
    #[serde(rename = "type")]
    block_type: BlockType,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub enum BlockType {
    Contains,
    #[default]
    Equals,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Trace {
    id: String,
    from_addr: Address,
    gas_limit: u128,
    input: Bytes,
    to_addr: Address,
    value: U256,
    gas_used: u128,
    output: Bytes,
    #[serde(rename = "type")]
    call_create_type: String,
    call_type: String,
    tx_id: String,
    parent_trace_id: String,
    pos_in_parent_trace: usize,
    self_storage_change: bool,
    storage_change: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Event {
    id: String,
    contract_id: Address,
    selector: String,
    topics: Vec<String>,
    data: Bytes,
    parent_trace_id: String,
    pos_in_parent_trace: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankMultiCallStats {
    /// blockNum
    pub block_num: u64,
    /// blockHash
    pub block_hash: H256,
    /// blockTime
    pub block_time: u64,
    /// success
    pub success: bool,
    /// cacheEnabled
    pub cache_enabled: bool,
}

/// Call Result
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankSingleCallResult {
    /// from
    pub code: i32,
    /// err
    pub err: String,
    /// fromCache
    pub from_cache: bool,
    /// result
    pub result: Bytes,
    /// gasUsed
    pub gas_used: i64,
    /// timeCost
    pub time_cost: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankMultiCallResp {
    /// results
    pub results: Vec<DebankSingleCallResult>,
    /// stats
    pub stats: DebankMultiCallStats,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankSimulateStats {
    /// blockNum
    pub block_num: u64,
    /// blockHash
    pub block_hash: H256,
    /// blockTime
    pub block_time: u64,
    /// success
    pub success: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankSingleSimulateResult {
    pub traces: Vec<Trace>,
    pub events: Vec<Event>,
    pub code: i32,
    pub err: String,
    pub gas_used: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankSimulateResp {
    pub results: Vec<DebankSingleSimulateResult>,
    pub stats: DebankSimulateStats,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankBlock {
    pub block_id: H256,
    pub block_height: u64,
    pub block_timestamp: u64,
}

#[cfg(test)]
mod tests {
    use super::super::BlockNumberOrTag;
    use super::*;

    #[test]
    fn test_block_context() {
        let block_id = BlockId::Number(BlockNumberOrTag::Latest);
        let block_context = BlockContext {
            block_id,
            block_type: BlockType::Contains,
        };
        let json = serde_json::to_string(&block_context).unwrap();
        assert_eq!(json, r#"{"block_id":"latest","type":"Contains"}"#);
    }
}
