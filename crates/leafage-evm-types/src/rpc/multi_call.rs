use crate::primitives::{Bytes, H256};
use serde::{Deserialize, Serialize};

/// Call Result
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SingleCallResult {
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
#[serde(rename_all = "camelCase")]
pub struct MultiCallStats {
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MultiCallResp {
    /// results
    pub results: Vec<SingleCallResult>,
    /// stats
    pub stats: MultiCallStats,
}

#[repr(i32)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiCallErrorCode {
    Success = 0,
    CodeTxArgs = -40000,
    NativeMethodNotFound = -40001,
    NativeMethodInput = -40002,
    NativeMethodInputAddress = -40003,
    NativeMethodOutput = -40010,
    NativeMethodStateError = -40011,
    MessageExecuting = -40012,
    EVMCancelled = -40013,
    EVMReverted = -40014,
    EVMFastFailed = -40015,
    UnderlyingDB = -40020,
    LoadingState = -40021,
}
