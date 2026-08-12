//! Wire types for the internal `blockx_stateReadBatch` method.
//!
//! The method is BlockX-internal: it is not exposed through leafage-py
//! or the BlockX audit allowlist, but it is still parsed as untrusted
//! network input. The serde shapes here are a cross-repo contract with
//! BlockX's batch facade — see `tests/fixtures/blockx_state_read_batch/`
//! in leafage-evm-rpc (mirrored in the BlockX repository); change them
//! only by adding a new method version.

use crate::{Address, Bytes, DebankBlockContext, JsonStorageKey, H256};
use serde::{Deserialize, Serialize};

/// Server-side hard cap on `reads` per batch. BlockX defaults to a
/// lower client-side cap (32); this bound is not request-controlled.
pub const BLOCKX_STATE_READ_BATCH_MAX_ITEMS: usize = 64;

/// One `blockx_stateReadBatch` request: a fixed block context plus the
/// state reads to resolve against that single state view.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BlockxStateReadBatch {
    #[serde(rename = "blockContext")]
    pub block_context: DebankBlockContext,
    pub reads: Vec<BlockxStateRead>,
}

/// A single logical read. `index` is the caller-assigned correlation id
/// echoed back in the matching [`BlockxStateReadOutcome`]; it must be
/// unique within a batch. Unknown `kind`s fail deserialization.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind")]
pub enum BlockxStateRead {
    /// Same semantics as `getAddressCode`.
    #[serde(rename = "addressCode")]
    AddressCode { index: u32, address: Address },
    /// Same semantics as `getStorageAt`.
    #[serde(rename = "storageAt")]
    StorageAt {
        index: u32,
        address: Address,
        position: JsonStorageKey,
    },
}

impl BlockxStateRead {
    pub fn index(&self) -> u32 {
        match self {
            BlockxStateRead::AddressCode { index, .. } => *index,
            BlockxStateRead::StorageAt { index, .. } => *index,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BlockxStateReadBatchResp {
    pub results: Vec<BlockxStateReadOutcome>,
}

/// Per-item outcome: exactly one of `value` / `error` is present.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BlockxStateReadOutcome {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<BlockxStateReadValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BlockxStateReadError>,
}

/// Item value: plain hex bytes on the wire, keeping the JSON shape of
/// the corresponding single method (`getStorageAt` -> 32-byte word,
/// `getAddressCode` -> variable-length code). Which shape applies is
/// determined by the request item's `kind`: a 32-byte contract code
/// and a storage word are indistinguishable on the wire, so this type
/// deliberately does not guess between them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct BlockxStateReadValue(pub Bytes);

impl BlockxStateReadValue {
    /// A `getStorageAt`-shaped value: the full 32-byte word.
    pub fn storage(word: H256) -> Self {
        Self(Bytes::copy_from_slice(word.as_slice()))
    }

    /// A `getAddressCode`-shaped value: raw code bytes (may be empty).
    pub fn code(code: Bytes) -> Self {
        Self(code)
    }
}

/// Item error carrying the single-method error `code` and `message`
/// verbatim. BlockX forwards both byte-for-byte to leafage-py, whose
/// -39006/-39007 handling parses the message text — never rewrite it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct BlockxStateReadError {
    pub code: i32,
    pub message: String,
}
