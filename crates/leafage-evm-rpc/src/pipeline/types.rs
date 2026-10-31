//! `trace_debankBlock` response types.
//!
//! Field names, order and JSON encodings follow the Go types of
//! `github.com/Chaintable/pipeline/types` (v0.0.69) that the pipeline nodes
//! (e.g. bsc-x) return:
//! `*big.Int` fields are JSON numbers, `hexutil.*` fields are hex strings
//! and addresses are lowercase hex.

use alloy::primitives::{Address, Bloom, Bytes, B256, B64, U256, U64};
use md5::{Digest as _, Md5};
use serde::Serialize;
use sha1::Sha1;

#[derive(Debug, Clone, Serialize)]
pub struct DebankBlockOutput {
    pub block_file: BlockFile,
    pub header: PipelineHeader,
    /// Left out when the request turned it off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_diff: Option<Bytes>,
    pub validation_hash: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BlockFile {
    pub block: PipelineBlock,
    pub txs: Vec<PipelineTransaction>,
    pub events: Vec<PipelineEvent>,
    pub traces: Vec<PipelineTrace>,
    pub error_events: Vec<PipelineEvent>,
    pub error_traces: Vec<PipelineTrace>,
    pub storage_contracts: Vec<String>,
}

impl BlockFile {
    /// `BlockFile.Validation().ValidationHash`: the block, tx, event and
    /// trace ids (error ones excluded).
    pub fn validation_hash(&self) -> i64 {
        let ids = std::iter::once(self.block.id.as_str())
            .chain(self.txs.iter().map(|tx| tx.id.as_str()))
            .chain(self.events.iter().map(|event| event.id.as_str()))
            .chain(self.traces.iter().map(|trace| trace.id.as_str()));
        calc_validation_hash(ids)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PipelineBlock {
    pub id: String,
    pub height: u64,
    pub parent_id: String,
    pub base_fee_per_gas: u128,
    pub miner: String,
    pub gas_limit: i64,
    pub gas_used: i64,
    pub timestamp: u64,
    pub process_start_timestamp: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PipelineTransaction {
    pub id: String,
    pub from_addr: String,
    pub to_addr: String,
    pub gas_limit: i64,
    pub gas_price: u128,
    pub gas_used: i64,
    pub status: bool,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub input: Bytes,
    pub nonce: i64,
    pub idx: i64,
    pub value: U256,
}

#[derive(Debug, Clone, Serialize)]
pub struct PipelineEvent {
    pub id: String,
    pub contract_id: String,
    pub selector: String,
    /// Go keeps `topics[1:]` of an empty slice as `nil`, i.e. `null`.
    pub topics: Option<Vec<String>>,
    pub data: Bytes,
    pub parent_trace_id: String,
    pub pos_in_parent_trace: i64,
    pub idx: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PipelineTrace {
    pub id: String,
    pub from_addr: String,
    pub gas_limit: i64,
    pub input: Bytes,
    pub to_addr: String,
    pub value: U256,
    /// Celo's unmetered Registry trace can wrap u64. Keep its positive JSON
    /// integer while also preserving the existing signed Go pipeline format.
    pub gas_used: i128,
    pub output: Bytes,
    #[serde(rename = "type")]
    pub call_create_type: String,
    pub call_type: String,
    pub tx_id: String,
    pub parent_trace_id: String,
    pub pos_in_parent_trace: i64,
    pub self_storage_change: bool,
    pub storage_change: bool,
    pub subtraces: i64,
    pub trace_address: Vec<i64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// `ptypes.Header`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineHeader {
    pub number: U256,
    pub hash: B256,
    pub parent_hash: B256,
    pub nonce: B64,
    pub mix_hash: B256,
    pub sha3_uncles: B256,
    pub logs_bloom: Bloom,
    pub state_root: B256,
    pub miner: String,
    pub difficulty: U256,
    pub extra_data: Bytes,
    pub gas_limit: U64,
    pub gas_used: U64,
    pub timestamp: U64,
    pub transactions_root: B256,
    pub receipts_root: B256,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_fee_per_gas: Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withdrawals_root: Option<B256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob_gas_used: Option<U64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excess_blob_gas: Option<U64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_beacon_block_root: Option<B256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requests_root: Option<B256>,
}

/// Go `common.Address.Hex()` lowercased.
pub fn addr_hex(address: &Address) -> String {
    format!("{address:#x}")
}

/// Go `common.Hash.Hex()`.
pub fn hash_hex(hash: &B256) -> String {
    format!("{hash:#x}")
}

/// `util.ToHash`: md5 over the concatenated arguments, lowercase hex.
pub fn to_hash(args: &[&str]) -> String {
    let mut hasher = Md5::new();
    for arg in args {
        hasher.update(arg.as_bytes());
    }
    alloy::hex::encode(hasher.finalize())
}

/// `types.CalcValidationHash`: sum the sha1 of every id as a big integer and
/// keep the last six decimal digits.
pub fn calc_validation_hash<'a>(ids: impl IntoIterator<Item = &'a str>) -> i64 {
    // 160-bit digests: the sum of 2^96 ids still fits in a U256.
    let mut sum = U256::ZERO;
    for id in ids {
        let digest = Sha1::digest(id.as_bytes());
        sum += U256::from_be_slice(&digest);
    }
    let digits = sum.to_string();
    digits[digits.len().saturating_sub(6)..]
        .parse()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_hash_matches_go_md5_concat() {
        // md5("0xab" + "" + "0")
        assert_eq!(
            to_hash(&["0xab", "", "0"]),
            alloy::hex::encode(Md5::digest(b"0xab0"))
        );
    }

    #[test]
    fn validation_hash_keeps_last_six_digits() {
        let digest = U256::from_be_slice(&Sha1::digest(b"a"));
        let digits = digest.to_string();
        let expected: i64 = digits[digits.len() - 6..].parse().unwrap();
        assert_eq!(calc_validation_hash(["a"]), expected);
    }

    #[test]
    fn empty_topics_serialize_as_null() {
        let event = PipelineEvent {
            id: String::new(),
            contract_id: String::new(),
            selector: String::new(),
            topics: None,
            data: Bytes::new(),
            parent_trace_id: String::new(),
            pos_in_parent_trace: 0,
            idx: 0,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json["topics"].is_null());
        assert_eq!(json["data"], "0x");
    }
}
