use super::{LocalizedTransactionTrace, Log};
use alloy::sol_types::decode_revert_reason;
use op_revm::OpHaltReason;
use revm::context::result::{ExecutionResult, HaltReason};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;

#[repr(i64)]
#[derive(Clone, Eq, PartialEq, Debug, Deserialize)]
pub enum PreErrorCode {
    UnKnown = 1000,
    InsufficientBalane = 1001,
    Reverted = 1002,
}

impl From<HaltReason> for PreErrorCode {
    fn from(reason: HaltReason) -> Self {
        match reason {
            HaltReason::OutOfFunds => PreErrorCode::InsufficientBalane,
            _ => PreErrorCode::UnKnown,
        }
    }
}

impl From<OpHaltReason> for PreErrorCode {
    fn from(reason: OpHaltReason) -> Self {
        match reason {
            OpHaltReason::Base(HaltReason::OutOfFunds) => PreErrorCode::InsufficientBalane,
            _ => PreErrorCode::UnKnown,
        }
    }
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

impl<T: Clone + Debug> From<ExecutionResult<T>> for PreResult
where
    PreErrorCode: From<T>,
{
    fn from(exec_res: ExecutionResult<T>) -> Self {
        match exec_res {
            ExecutionResult::Revert { gas, output, .. } => {
                let reason =
                    decode_revert_reason(&output).unwrap_or("execution revert".to_string());
                let pre_error = PreError {
                    msg: reason,
                    code: PreErrorCode::Reverted as i64,
                };
                let pre_res = PreResult {
                    error: pre_error,
                    gas_used: gas.used(),
                    ..Default::default()
                };
                pre_res
            }
            ExecutionResult::Halt { reason, gas, .. } => {
                let code = PreErrorCode::from(reason.clone()) as i64;
                let pre_error = PreError {
                    msg: format!("{:?}", reason),
                    code,
                };
                let pre_res = PreResult {
                    error: pre_error,
                    gas_used: gas.used(),
                    ..Default::default()
                };
                pre_res
            }
            ExecutionResult::Success { gas, logs, .. } => {
                let mut trace_logs = vec![];
                let mut log_index = 0;
                for log in logs {
                    trace_logs.push(Log {
                        inner: log,
                        log_index: Some(log_index),
                        removed: false,
                        ..Default::default()
                    });
                    log_index += 1;
                }
                let pre_res = PreResult {
                    gas_used: gas.used(),
                    logs: trace_logs,
                    ..Default::default()
                };
                pre_res
            }
        }
    }
}
