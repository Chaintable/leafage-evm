use crate::primitives::{Bytes, H256};
use alloy::sol_types::decode_revert_reason;
use revm::context::result::ExecutionResult;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;

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

impl<T: Clone + Debug> From<ExecutionResult<T>> for SingleCallResult {
    fn from(exec_res: ExecutionResult<T>) -> Self {
        let res = match exec_res {
            ExecutionResult::Success {
                output, gas_used, ..
            } => SingleCallResult {
                code: MultiCallErrorCode::Success as i32,
                err: "".to_string(),
                from_cache: false,
                result: output.into_data().0.into(),
                gas_used: gas_used as i64,
                time_cost: 0.0,
            },
            ExecutionResult::Revert {
                output, gas_used, ..
            } => SingleCallResult {
                code: MultiCallErrorCode::EVMReverted as i32,
                err: decode_revert_reason(&output).unwrap_or("execution revert".to_string()),
                from_cache: false,
                result: Bytes::default(),
                gas_used: gas_used as i64,
                time_cost: 0.0,
            },
            ExecutionResult::Halt { reason, gas_used } => SingleCallResult {
                code: MultiCallErrorCode::EVMCancelled as i32,
                err: format!("Halted: {:?}", reason),
                from_cache: false,
                result: Bytes::default(),
                gas_used: gas_used as i64,
                time_cost: 0.0,
            },
        };
        res
    }
}
