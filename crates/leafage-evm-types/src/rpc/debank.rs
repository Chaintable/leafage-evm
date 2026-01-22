use crate::{Address, Block, BlockId, Bytes, H256, U256};
use alloy::primitives::{BlockHash, TxKind};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::decode_revert_reason;
use op_revm::OpHaltReason;
use revm::context::result::{ExecutionResult, HaltReason};
use revm_bytecode::opcode::OpCode;
use revm_inspectors::tracing::types::{CallKind, CallLog, CallTraceNode};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::sync::Arc;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankBlockContext {
    pub block_id: BlockId,
    #[serde(rename = "type")]
    pub block_type: BlockType,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum BlockType {
    Contains,
    #[default]
    Equals,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankTrace {
    pub id: String,
    pub from_addr: Address,
    pub gas_limit: u64,
    pub input: Bytes,
    pub to_addr: Address,
    pub value: U256,
    pub gas_used: u64,
    pub output: Bytes,
    #[serde(rename = "type")]
    pub call_create_type: String,
    pub call_type: String,
    pub tx_id: H256,
    pub parent_trace_id: String,
    pub pos_in_parent_trace: usize,
    pub self_storage_change: bool,
    pub storage_change: bool,
}

impl DebankID for DebankTrace {
    fn debank_id(&self) -> String {
        Self::calculate_id(vec![
            &self.tx_id.to_string(),
            &self.parent_trace_id,
            &self.pos_in_parent_trace.to_string(),
        ])
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankEvent {
    pub id: String,
    pub contract_id: Address,
    pub selector: String,
    pub topics: Vec<String>,
    pub data: Bytes,
    pub tx_id: H256,
    pub parent_trace_id: String,
    pub pos_in_parent_trace: usize,
}

impl DebankID for DebankEvent {
    fn debank_id(&self) -> String {
        Self::calculate_id(vec![
            &self.parent_trace_id,
            &self.pos_in_parent_trace.to_string(),
        ])
    }
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

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DebankErrorCode {
    #[allow(dead_code)]
    InvalidJson = -32700,
    #[allow(dead_code)]
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    EvmRevert = -39000,
    GasExhausted = -39001,
    BalanceExhausted = -39002,
    NonceError = -39003,
    EvmFailed = -39004,
    DataBaseFailed = -39005,
    BlockNotFound = -39006,
    InvalidBlockID = -39007,
    UnsupportedPrecompile = -39008,
    #[allow(dead_code)]
    InternalError = -32603,
    TimeOut = -41002,
}

impl From<HaltReason> for DebankErrorCode {
    fn from(reason: HaltReason) -> Self {
        match reason {
            HaltReason::OutOfFunds => DebankErrorCode::BalanceExhausted,
            HaltReason::NonceOverflow => DebankErrorCode::NonceError,
            HaltReason::OutOfGas(_) => DebankErrorCode::GasExhausted,
            _ => DebankErrorCode::EvmFailed,
        }
    }
}

impl From<OpHaltReason> for DebankErrorCode {
    fn from(err: OpHaltReason) -> Self {
        match err {
            op_revm::result::OpHaltReason::Base(HaltReason::OutOfFunds) => {
                DebankErrorCode::BalanceExhausted
            }
            op_revm::result::OpHaltReason::Base(HaltReason::NonceOverflow) => {
                DebankErrorCode::NonceError
            }
            op_revm::result::OpHaltReason::Base(HaltReason::OutOfGas(_)) => {
                DebankErrorCode::GasExhausted
            }
            _ => DebankErrorCode::EvmFailed,
        }
    }
}

impl<T: Clone + Debug> From<ExecutionResult<T>> for DebankSingleCallResult
where
    DebankErrorCode: From<T>,
{
    fn from(exec_res: ExecutionResult<T>) -> Self {
        let res = match exec_res {
            ExecutionResult::Success {
                output, gas_used, ..
            } => DebankSingleCallResult {
                code: 0,
                err: "".to_string(),
                from_cache: false,
                result: output.into_data().0.into(),
                gas_used: gas_used as i64,
                time_cost: 0.0,
            },
            ExecutionResult::Revert {
                output, gas_used, ..
            } => DebankSingleCallResult {
                code: DebankErrorCode::EvmRevert as i32,
                err: decode_revert_reason(&output).unwrap_or("execution revert".to_string()),
                from_cache: false,
                result: Bytes::default(),
                gas_used: gas_used as i64,
                time_cost: 0.0,
            },
            ExecutionResult::Halt { reason, gas_used } => DebankSingleCallResult {
                code: DebankErrorCode::from(reason.clone()) as i32,
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
    pub traces: Vec<DebankTrace>,
    pub events: Vec<DebankEvent>,
    pub code: i32,
    pub err: String,
    pub gas_used: u64,
}

impl<T: Clone + Debug> From<ExecutionResult<T>> for DebankSingleSimulateResult
where
    DebankErrorCode: From<T>,
{
    fn from(exec_res: ExecutionResult<T>) -> Self {
        match exec_res {
            ExecutionResult::Revert { gas_used, output } => {
                let reason =
                    decode_revert_reason(&output).unwrap_or("execution revert".to_string());
                let pre_res = DebankSingleSimulateResult {
                    code: DebankErrorCode::EvmRevert as i32,
                    err: reason,
                    gas_used,
                    ..Default::default()
                };
                pre_res
            }
            ExecutionResult::Halt { reason, gas_used } => {
                let code = DebankErrorCode::from(reason.clone());
                let pre_res = DebankSingleSimulateResult {
                    code: code as i32,
                    err: format!("Halted: {:?}", reason),
                    gas_used,
                    ..Default::default()
                };
                pre_res
            }
            ExecutionResult::Success { gas_used, .. } => {
                let pre_res = DebankSingleSimulateResult {
                    gas_used,
                    ..Default::default()
                };
                pre_res
            }
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankSimulateResp {
    pub results: Vec<DebankSingleSimulateResult>,
    pub stats: DebankSimulateStats,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DebankBlock {
    pub id: H256,
    pub height: u64,
    pub timestamp: u64,
    pub parent_id: H256,
    pub base_fee_per_gas: u64,
    pub miner: Address,
    pub gas_limit: u64,
    pub gas_used: u64,
}

impl<T> From<Arc<Block<T>>> for DebankBlock {
    fn from(block: Arc<Block<T>>) -> Self {
        DebankBlock {
            id: block.header.hash,
            height: block.header.number,
            timestamp: block.header.timestamp,
            parent_id: block.header.parent_hash,
            base_fee_per_gas: block.header.base_fee_per_gas.unwrap_or_default(),
            miner: block.header.beneficiary,
            gas_limit: block.header.gas_limit,
            gas_used: block.header.gas_used,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(default)]
pub struct DebankTransaction {
    pub id: BlockHash,
    #[serde(rename = "from_addr")]
    pub from: Address,
    #[serde(rename = "to_addr")]
    pub to: Address,
    pub gas_limit: u64,
    pub gas_price: u128,
    pub gas_used: u64,
    pub status: bool,
    #[serde(rename = "max_fee_per_gas")]
    pub gas_fee_cap: u128,
    #[serde(rename = "max_priority_fee_per_gas")]
    pub gas_tip_cap: u128,
    pub input: Bytes,
    pub nonce: u64,
    #[serde(rename = "idx")]
    pub transaction_index: u64,
    pub value: U256,
}

impl Into<TransactionRequest> for DebankTransaction {
    fn into(self) -> TransactionRequest {
        TransactionRequest {
            from: self.from.into(),
            to: Some(if self.to.is_empty() {
                TxKind::Create
            } else {
                TxKind::Call(self.to)
            }),
            gas_price: self.gas_price.into(),
            max_fee_per_gas: self.gas_fee_cap.into(),
            max_priority_fee_per_gas: self.gas_tip_cap.into(),
            max_fee_per_blob_gas: None,
            gas: self.gas_limit.into(),
            value: self.value.into(),
            input: self.input.into(),
            nonce: self.nonce.into(),
            chain_id: None,
            access_list: None,
            transaction_type: None,
            blob_versioned_hashes: None,
            sidecar: None,
            authorization_list: None,
        }
    }
}

pub trait DebankID {
    fn debank_id(&self) -> String;

    fn calculate_id(args: Vec<&str>) -> String {
        use md5::{Digest, Md5};
        let mut hasher = Md5::new();
        for arg in args {
            hasher.update(arg.as_bytes());
        }
        let result = hasher.finalize();
        format!("{:x}", result)
    }
}

impl From<&CallTraceNode> for DebankTrace {
    fn from(call_trace: &CallTraceNode) -> Self {
        let trace = &call_trace.trace;
        let mut debank_trace = DebankTrace {
            id: "".to_string(),
            from_addr: trace.caller,
            gas_limit: trace.gas_limit,
            input: trace.data.clone(),
            to_addr: trace.address,
            value: trace.value,
            gas_used: trace.gas_used,
            output: trace.output.clone(),
            call_create_type: match trace.kind {
                CallKind::Call
                | CallKind::StaticCall
                | CallKind::CallCode
                | CallKind::DelegateCall
                | CallKind::AuthCall => "call".to_string(),
                CallKind::Create | CallKind::Create2 => "create".to_string(),
            },
            call_type: match trace.kind {
                CallKind::Call
                | CallKind::StaticCall
                | CallKind::CallCode
                | CallKind::DelegateCall
                | CallKind::AuthCall => "call".to_string(),
                _ => "".to_string(),
            },
            ..Default::default()
        };
        if call_trace.is_selfdestruct() {
            debank_trace.call_create_type = "suicide".to_string();
        }
        for op in trace.steps.iter() {
            if op.op == OpCode::SSTORE {
                debank_trace.self_storage_change = true;
                debank_trace.storage_change = true;
            }
        }
        debank_trace
    }
}

impl From<&CallLog> for DebankEvent {
    fn from(log: &CallLog) -> Self {
        let selector = log
            .raw_log
            .topics()
            .first()
            .map(|h| h.to_string())
            .unwrap_or_default();
        let topics = if log.raw_log.topics().len() > 1 {
            log.raw_log.topics()[1..]
                .iter()
                .map(|h| h.to_string())
                .collect()
        } else {
            vec![]
        };

        DebankEvent {
            selector,
            topics,
            data: log.raw_log.data.clone(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::BlockNumberOrTag;
    use super::*;

    #[test]
    fn test_block_context() {
        let block_id = BlockId::Number(BlockNumberOrTag::Latest);
        let block_context = DebankBlockContext {
            block_id,
            block_type: BlockType::Contains,
        };
        let json = serde_json::to_string(&block_context).unwrap();
        assert_eq!(json, r#"{"block_id":"latest","type":"Contains"}"#);
    }

    #[test]
    fn test_to_debank_id() {
        struct IDChecker;

        impl DebankID for IDChecker {
            fn debank_id(&self) -> String {
                Self::calculate_id(vec!["abcd", "2"])
            }
        }

        assert_eq!(
            IDChecker {}.debank_id(),
            "6e24a85785fd5e2688f1a23aee9d88f3".to_string()
        );
    }
}
