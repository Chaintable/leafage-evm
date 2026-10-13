//! The four bundle resources of `pipeline-compactor`, built in memory.
//!
//! | Resource   | Target object    | Format                          |
//! |------------|------------------|---------------------------------|
//! | Validation | `<b>/validation` | gzip JSON `BlockValidation`     |
//! | BlockFile  | `<b>`            | gzip JSON array of block files  |
//! | Header     | `<b>/block`      | gzip JSON array of headers      |
//! | StateDiff  | `<b>/stateDiff`  | fixed index, then RLP diffs     |
//!
//! The block file bundle is the large one (hundreds of MB compressed): it is
//! compressed as the blocks are replayed and handed out in parts to upload.

use super::chain::ReplayOutput;
use super::types::BlockFile;
use alloy::primitives::B256;
use anyhow::{bail, Context as _, Result};
use flate2::write::GzEncoder;
use leafage_evm_types::{BlockStorageDiff, BundleStorageDiffIndex, STATE_DIFF_INDEX_BYTES};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
use std::str::FromStr;

/// Blocks per bundle.
pub const BUNDLE_SIZE: u64 = 1000;

/// A bundle resource, ordered as the compactor writes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Resource {
    Validation,
    BlockFile,
    Header,
    StateDiff,
}

impl Resource {
    pub const ALL: [Resource; 4] = [
        Resource::Validation,
        Resource::BlockFile,
        Resource::Header,
        Resource::StateDiff,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Resource::Validation => "validation",
            Resource::BlockFile => "blockfile",
            Resource::Header => "header",
            Resource::StateDiff => "statediff",
        }
    }
}

impl std::fmt::Display for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Resource {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Resource::ALL
            .into_iter()
            .find(|resource| resource.name() == s.trim().to_ascii_lowercase())
            .with_context(|| {
                format!(
                    "unknown resource {s:?}, expected validation, blockfile, header or statediff"
                )
            })
    }
}

/// pipeline `BlockValidation`, summed over a bundle as the compactor does.
#[derive(Debug, Default, Serialize)]
pub(crate) struct BundleValidation {
    validation_hash: i64,
    is_fork: bool,
    txs_count: u64,
    events_count: u64,
    traces_count: u64,
    error_events_count: u64,
    error_traces_count: u64,
    storage_contracts_count: u64,
}

impl BundleValidation {
    fn add(&mut self, file: &BlockFile) {
        self.validation_hash = (self.validation_hash + file.validation_hash()) % 1_000_000;
        self.txs_count += file.txs.len() as u64;
        self.events_count += file.events.len() as u64;
        self.traces_count += file.traces.len() as u64;
        self.error_events_count += file.error_events.len() as u64;
        self.error_traces_count += file.error_traces.len() as u64;
        self.storage_contracts_count += file.storage_contracts.len() as u64;
    }
}

/// A JSON array gzip-compressed element by element into memory.
struct GzJsonArray {
    encoder: GzEncoder<Vec<u8>>,
    /// Each element is serialized whole before compressing: serde writes
    /// many small pieces, each a costly call into the encoder otherwise.
    json: Vec<u8>,
    empty: bool,
}

impl GzJsonArray {
    fn new(level: u32) -> Self {
        Self {
            encoder: GzEncoder::new(Vec::new(), flate2::Compression::new(level)),
            json: Vec::new(),
            empty: true,
        }
    }

    fn push<T: Serialize>(&mut self, value: &T) -> Result<()> {
        self.json.clear();
        self.json.push(if std::mem::take(&mut self.empty) {
            b'['
        } else {
            b','
        });
        serde_json::to_writer(&mut self.json, value)?;
        self.encoder.write_all(&self.json)?;
        Ok(())
    }

    /// The compressed output so far, once there is at least `min` of it.
    fn take_output(&mut self, min: usize) -> Option<Vec<u8>> {
        let output = self.encoder.get_mut();
        (output.len() >= min).then(|| std::mem::take(output))
    }

    /// The rest of the compressed output.
    fn finish(mut self) -> Result<Vec<u8>> {
        self.encoder
            .write_all(if self.empty { b"[]" } else { b"]" })?;
        Ok(self.encoder.finish()?)
    }
}

/// The compactor's StateDiff bundle: a fixed index of a bitmap and payload
/// offsets, then the RLP `BlockStorageDiff`s.
#[derive(Default)]
struct StateDiffWriter {
    /// Whether each entry is a real diff rather than the empty one standing
    /// for an unchanged state root.
    real: Vec<bool>,
    /// Payload end of each entry.
    ends: Vec<u64>,
    payload: Vec<u8>,
}

impl StateDiffWriter {
    /// Add the diff of the block with `state_root`, empty when the state
    /// root did not change.
    fn push(&mut self, diff: &[u8], state_root: B256) {
        if diff.is_empty() {
            // The compactor's stand-in for an unchanged state root.
            let empty = BlockStorageDiff {
                hash: state_root,
                parent_hash: state_root,
                ..Default::default()
            };
            alloy::rlp::Encodable::encode(&empty, &mut self.payload);
            self.real.push(false);
        } else {
            self.payload.extend_from_slice(diff);
            self.real.push(true);
        }
        self.ends.push(self.payload.len() as u64);
    }

    fn finish(self) -> Result<Vec<u8>> {
        if self.ends.len() as u64 != BUNDLE_SIZE {
            bail!(
                "a StateDiff bundle needs {BUNDLE_SIZE} entries, got {}",
                self.ends.len()
            );
        }
        let mut data = Vec::with_capacity(STATE_DIFF_INDEX_BYTES + self.payload.len());
        let mut bitmap = [0u8; 125];
        for (position, _) in self.real.iter().enumerate().filter(|(_, real)| **real) {
            bitmap[position / 8] |= 1 << (position % 8);
        }
        data.extend_from_slice(&bitmap);
        data.extend_from_slice(&0u64.to_be_bytes());
        for end in &self.ends {
            data.extend_from_slice(&end.to_be_bytes());
        }
        // A sanity check by the reader of the format.
        BundleStorageDiffIndex::decode_for_bundle(1, &data)
            .map_err(|err| anyhow::anyhow!("invalid StateDiff index: {err:?}"))?;
        data.extend_from_slice(&self.payload);
        Ok(data)
    }
}

/// The finished objects of a bundle; the block file only has what was not
/// taken out with [`BundleWriter::take_block_file_part`].
#[derive(Default)]
pub(crate) struct BundleObjects {
    pub validation: Option<Vec<u8>>,
    pub block_file: Option<Vec<u8>>,
    pub header: Option<Vec<u8>>,
    pub state_diff: Option<Vec<u8>>,
}

/// Builds the chosen resources of one bundle as its blocks are replayed.
pub(crate) struct BundleWriter {
    gzip_level: u32,
    block_file: Option<GzJsonArray>,
    validation: Option<BundleValidation>,
    headers: Option<GzJsonArray>,
    state_diff: Option<StateDiffWriter>,
}

impl BundleWriter {
    pub fn new(resources: &BTreeSet<Resource>, gzip_level: u32) -> Self {
        let has = |resource| resources.contains(&resource);
        Self {
            gzip_level,
            block_file: has(Resource::BlockFile).then(|| GzJsonArray::new(gzip_level)),
            validation: has(Resource::Validation).then(BundleValidation::default),
            headers: has(Resource::Header).then(|| GzJsonArray::new(gzip_level)),
            state_diff: has(Resource::StateDiff).then(StateDiffWriter::default),
        }
    }

    /// Add the next block of the bundle.
    pub fn push(&mut self, output: &ReplayOutput) -> Result<()> {
        if let Some(writer) = &mut self.block_file {
            writer.push(&output.block_file)?;
        }
        if let Some(validation) = &mut self.validation {
            validation.add(&output.block_file);
        }
        if let Some(writer) = &mut self.headers {
            writer.push(&output.header)?;
        }
        if let Some(writer) = &mut self.state_diff {
            let diff = output
                .state_diff
                .as_ref()
                .context("the replay produced no state diff")?;
            writer.push(diff, output.header.state_root);
        }
        Ok(())
    }

    /// The compressed block file so far, once there is at least `min` of it,
    /// to upload as a part.
    pub fn take_block_file_part(&mut self, min: usize) -> Option<Vec<u8>> {
        self.block_file.as_mut()?.take_output(min)
    }

    pub fn finish(self) -> Result<BundleObjects> {
        let validation = match self.validation {
            Some(validation) => {
                let mut encoder =
                    GzEncoder::new(Vec::new(), flate2::Compression::new(self.gzip_level));
                serde_json::to_writer(&mut encoder, &validation)?;
                Some(encoder.finish()?)
            }
            None => None,
        };
        Ok(BundleObjects {
            validation,
            block_file: self.block_file.map(GzJsonArray::finish).transpose()?,
            header: self.headers.map(GzJsonArray::finish).transpose()?,
            state_diff: self.state_diff.map(StateDiffWriter::finish).transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_types::BundleStorageDiff;
    use std::io::Read;

    #[test]
    fn state_diff_bundle_decodes() {
        let mut writer = StateDiffWriter::default();
        let real = BlockStorageDiff {
            hash: B256::repeat_byte(2),
            parent_hash: B256::repeat_byte(1),
            ..Default::default()
        };
        for position in 0..BUNDLE_SIZE {
            if position % 3 == 0 {
                writer.push(&[], B256::repeat_byte(7));
            } else {
                writer.push(&alloy::rlp::encode(&real), B256::ZERO);
            }
        }
        let data = writer.finish().unwrap();
        BundleStorageDiff::decode(&data).unwrap();
        let index = BundleStorageDiffIndex::decode(&data[..STATE_DIFF_INDEX_BYTES]).unwrap();
        assert!(!index.has_source_diff(0).unwrap());
        assert!(index.has_source_diff(1).unwrap());
        assert!(!index.has_source_diff(999).unwrap());
    }

    #[test]
    fn gzip_json_array_in_parts() {
        let mut array = GzJsonArray::new(6);
        let mut compressed = Vec::new();
        for i in 0..20_000u64 {
            array.push(&[i, i * 7]).unwrap();
            if let Some(part) = array.take_output(4096) {
                compressed.extend(part);
            }
        }
        compressed.extend(array.finish().unwrap());
        let mut json = String::new();
        flate2::read::GzDecoder::new(&compressed[..])
            .read_to_string(&mut json)
            .unwrap();
        let values: Vec<[u64; 2]> = serde_json::from_str(&json).unwrap();
        assert_eq!(values.len(), 20_000);
        assert_eq!(values[19_999], [19_999, 139_993]);
        assert_eq!(
            serde_json::from_slice::<Vec<u64>>(&gunzip(&GzJsonArray::new(6).finish().unwrap()))
                .unwrap(),
            Vec::<u64>::new()
        );
    }

    fn gunzip(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(data)
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn resources_parse_and_order() {
        let resources: BTreeSet<Resource> = ["statediff", "blockfile", "Validation"]
            .iter()
            .map(|name| name.parse().unwrap())
            .collect();
        assert_eq!(
            resources.into_iter().collect::<Vec<_>>(),
            [
                Resource::Validation,
                Resource::BlockFile,
                Resource::StateDiff
            ]
        );
        assert!("trace".parse::<Resource>().is_err());
    }
}
