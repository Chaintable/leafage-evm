use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use leafage_evm_types::{Bytes, CallRequest, U256};

/// The full corpus file (`corpus.json`).
#[derive(Debug, Clone, Deserialize)]
pub struct Corpus {
    pub meta: CorpusMeta,
    pub cases: Vec<CorpusCase>,
}

impl Corpus {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path)?;
        let corpus = serde_json::from_reader(file)?;
        Ok(corpus)
    }
}
/// Meta
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusMeta {
    pub generated_at: String,
    pub format: String,
    pub seed: String,
    pub group_cap: u32,
    pub selector_cap: u32,
    /// Per-label case quotas used during balanced sampling, e.g. `{"L1": 300, "L2": 300, "L3": 100}`.
    pub quotas: HashMap<String, u32>,
    pub ingest_stats: IngestStats,
    pub stage: String,
    pub case_count: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct IngestStats {
    pub requests_received: u64,
    pub cases_ingested: u64,
    pub rpc_objects_found: u64,
}

/// Case
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusCase {
    /// SHA-256 derived 24-char hex identifier, unique per `(chainId, blockNumber, request)`.
    pub case_id: String,
    /// Always `"eth_call"` in this corpus.
    pub rpc_method: String,
    /// The normalised call request sent to `eth_call`.
    pub request: CallRequest,
    /// Fixed block number in hex (e.g. `"0x1787040"`).
    pub block_number: U256,
    /// The original block reference before resolution (may be `"latest"`, a struct, etc.).
    pub original_block: Option<String>,
    /// The source RPC method before explosion, e.g. `"contractMultiCall"`.
    pub original_rpc_method: String,
    /// Index inside the original batch request, `None` for single `eth_call`.
    pub batch_index: Option<u32>,
    /// 4-byte function selector extracted from `request.data`, e.g. `"0x70a08231"`.
    pub selector: Option<Bytes>,
    /// Classification tags, e.g. `["contractMultiCall", "scenario:token_balance"]`.
    /// Internal biz/source tags have been stripped before publication.
    pub tags: Vec<String>,
    /// EIP-155 chain ID in hex, e.g. `"0x1"` for Ethereum mainnet.
    pub chain_id: U256,
    /// Complexity classification assigned by the scoring model.
    pub classification: Classification,
}

/// Complexity tier and scoring details assigned by the signal model.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Classification {
    /// Complexity label: `"L1"` (light), `"L2"` (medium), or `"L3"` (heavy).
    pub label: ClassLabel,
    /// Raw score computed by the signal model. Lower = lighter.
    pub score: i32,
    /// Individual signals that contributed to the score.
    pub signals: Vec<String>,
    /// Byte length of `request.data`.
    pub data_len_bytes: Option<usize>,
    /// Whether the request includes a `from` field.
    pub has_from: bool,
}

/// Complexity tier of an `eth_call` case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum ClassLabel {
    /// Lightweight read: simple getter, ≤ 1 argument, or known cheap selector
    /// (`balanceOf`, `totalSupply`, `decimals`, …).
    L1,
    /// Medium complexity: unknown or moderately complex selector, standard view call.
    L2,
    /// Heavy call: long calldata, state-dependent (`from` present), or complex view function.
    L3,
}

impl ClassLabel {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClassLabel::L1 => "L1",
            ClassLabel::L2 => "L2",
            ClassLabel::L3 => "L3",
        }
    }
}

impl std::fmt::Display for ClassLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_serde() {
        Corpus::load(Path::new("corpus/corpus.json")).unwrap();
    }
}
