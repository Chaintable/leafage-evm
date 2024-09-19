use super::{LocalizedTransactionTrace, Log};
use serde::{Deserialize, Serialize};

#[repr(i64)]
#[derive(Clone, Eq, PartialEq, Debug, Deserialize)]
pub enum PreErrorCode {
    UnKnown = 1000,
    InsufficientBalane = 1001,
    Reverted = 1002,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct PreError {
    pub code: i64,
    pub msg: String,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct PreResult {
    pub trace: Vec<LocalizedTransactionTrace>,
    pub logs: Vec<Log>,
    pub error: PreError,
    #[serde(rename = "gasUsed")]
    pub gas_used: u64,
}
