use std::sync::Arc;
use super::{
    super::primitives::{Address, Bytes, H256, U256},
    BlockId,
};
use revm::interpreter::OpCode;
use revm_inspectors::tracing::types::{CallKind, CallLog, CallTraceNode};
use serde::{Deserialize, Serialize};
use crate::{Block, Transaction};

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
    pub parent_hash: H256,
    pub base_fee_per_gas: u64,
    pub miner: Address,
    pub gas_limit: u64,
    pub gas_used: u64,
}

impl From<Arc<Block<Transaction>>> for DebankBlock {
    fn from(block: Arc<Block<Transaction>>) -> Self {
        DebankBlock {
            block_id: block.header.hash,
            block_height: block.header.number,
            block_timestamp: block.header.timestamp,
            parent_hash: block.header.parent_hash,
            base_fee_per_gas: block.header.base_fee_per_gas.unwrap_or_default(),
            miner: block.header.beneficiary,
            gas_limit: block.header.gas_limit,
            gas_used: block.header.gas_used,
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
                CallKind::Create | CallKind::Create2 | CallKind::EOFCreate => "create".to_string(),
            },
            call_type: trace.kind.to_string(),
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
            .get(0)
            .map(|h| h.to_string())
            .unwrap_or_default();
        let topics = if log.raw_log.topics().len() > 1 {
            log.raw_log.topics()[1..]
                .into_iter()
                .map(|h| h.to_string())
                .collect()
        } else {
            vec![]
        };
        let event = DebankEvent {
            selector,
            topics,
            data: log.raw_log.data.clone(),
            ..Default::default()
        };
        event
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
