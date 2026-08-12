//! Wire types for the internal `blockx_stateReadBatch` method (BSRB/1).
//!
//! The method carries a compact binary payload inside the ordinary
//! JSON-RPC shell: `params` is one hex-encoded byte string and the
//! result is another. The layout is a cross-repo contract with BlockX's
//! executor-side provider (which packs it with `struct.pack` and parses
//! it with `memoryview`); the golden byte vectors live in
//! `tests/blockx_wire_contract.rs` in leafage-evm-rpc and are mirrored
//! in the BlockX repository. Evolve it only by bumping [`BSRB_VERSION`].
//!
//! All integers are big-endian. Request:
//!
//! ```text
//! u8  version        == 1
//! u8  ctx_kind       0 = block hash, 1 = block height   (Equals semantics)
//! 32B / 8B           the hash, or the height as u64
//! u16 count          1..=BLOCKX_STATE_READ_BATCH_MAX_ITEMS
//! per item:
//!   u8  kind         0 = addressCode, 1 = storageAt
//!   20B address
//!   32B slot         (storageAt only)
//! ```
//!
//! Response — item order mirrors the request (order is the correlation;
//! there are no per-item indexes):
//!
//! ```text
//! u8  version        == 1
//! u16 count          == request count
//! per item:
//!   u8  tag          0 = ok, 1 = error
//!   ok:  u32 len + payload      (storageAt: the full 32-byte word;
//!                                addressCode: raw code bytes, may be empty)
//!   err: i32 code + u32 msg_len + msg   (single-method error code and
//!                                        byte-exact message text)
//! ```
//!
//! Trailing bytes, truncation, unknown versions/kinds/tags and
//! out-of-range counts all fail decoding: the payload is parsed as
//! untrusted network input even though the method is BlockX-internal.

use crate::{Address, Bytes, H256};

/// Server-side hard cap on reads per batch. BlockX defaults to a lower
/// client-side cap (32); this bound is not request-controlled.
pub const BLOCKX_STATE_READ_BATCH_MAX_ITEMS: usize = 64;

/// Wire version of the BSRB payload; bump on any layout change.
pub const BSRB_VERSION: u8 = 1;

const CTX_KIND_HASH: u8 = 0;
const CTX_KIND_NUMBER: u8 = 1;
const READ_KIND_ADDRESS_CODE: u8 = 0;
const READ_KIND_STORAGE_AT: u8 = 1;
const TAG_OK: u8 = 0;
const TAG_ERROR: u8 = 1;

/// The fixed block the whole batch resolves against (`Equals`
/// semantics). Dynamic contexts (`latest`, `Contains`) stay on the
/// single-method path and are unrepresentable here by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BsrbContext {
    Hash(H256),
    Number(u64),
}

/// One logical read; response items answer in request order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BsrbRead {
    /// Same semantics as `getAddressCode`.
    AddressCode { address: Address },
    /// Same semantics as `getStorageAt`.
    StorageAt { address: Address, slot: H256 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BsrbRequest {
    pub context: BsrbContext,
    pub reads: Vec<BsrbRead>,
}

/// Per-item outcome. `Error` carries the single-method error `code` and
/// `message` verbatim: BlockX's provider forwards both to leafage-py,
/// whose -39006/-39007 handling parses the message text — never rewrite
/// it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BsrbOutcome {
    /// `storageAt`: the full 32-byte word. `addressCode`: raw code
    /// bytes (empty for codeless accounts). Which shape applies is
    /// determined by the request item at the same position.
    Value(Bytes),
    Error {
        code: i32,
        message: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BsrbResponse {
    pub results: Vec<BsrbOutcome>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BsrbDecodeError {
    #[error("payload truncated: need {need} more byte(s) at offset {offset}")]
    Truncated { offset: usize, need: usize },
    #[error("unsupported BSRB version {0}, expected {BSRB_VERSION}")]
    Version(u8),
    #[error("unknown block context kind {0}")]
    ContextKind(u8),
    #[error("unknown read kind {0} at item {1}")]
    ReadKind(u8, usize),
    #[error("unknown outcome tag {0} at item {1}")]
    OutcomeTag(u8, usize),
    #[error("item count {0} outside 1..={BLOCKX_STATE_READ_BATCH_MAX_ITEMS}")]
    ItemCount(usize),
    #[error("{0} trailing byte(s) after the last item")]
    TrailingBytes(usize),
    #[error("error message at item {0} is not valid UTF-8")]
    MessageUtf8(usize),
}

struct Cursor<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], BsrbDecodeError> {
        let end = self
            .offset
            .checked_add(len)
            .filter(|&e| e <= self.data.len());
        let Some(end) = end else {
            return Err(BsrbDecodeError::Truncated {
                offset: self.offset,
                need: self.offset + len - self.data.len(),
            });
        };
        let out = &self.data[self.offset..end];
        self.offset = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, BsrbDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, BsrbDecodeError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, BsrbDecodeError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, BsrbDecodeError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, BsrbDecodeError> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn finish(self) -> Result<(), BsrbDecodeError> {
        match self.data.len() - self.offset {
            0 => Ok(()),
            n => Err(BsrbDecodeError::TrailingBytes(n)),
        }
    }
}

impl BsrbRequest {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 33 + self.reads.len() * 53);
        out.push(BSRB_VERSION);
        match &self.context {
            BsrbContext::Hash(hash) => {
                out.push(CTX_KIND_HASH);
                out.extend_from_slice(hash.as_slice());
            }
            BsrbContext::Number(number) => {
                out.push(CTX_KIND_NUMBER);
                out.extend_from_slice(&number.to_be_bytes());
            }
        }
        out.extend_from_slice(&(self.reads.len() as u16).to_be_bytes());
        for read in &self.reads {
            match read {
                BsrbRead::AddressCode { address } => {
                    out.push(READ_KIND_ADDRESS_CODE);
                    out.extend_from_slice(address.as_slice());
                }
                BsrbRead::StorageAt { address, slot } => {
                    out.push(READ_KIND_STORAGE_AT);
                    out.extend_from_slice(address.as_slice());
                    out.extend_from_slice(slot.as_slice());
                }
            }
        }
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self, BsrbDecodeError> {
        let mut cursor = Cursor::new(data);
        let version = cursor.u8()?;
        if version != BSRB_VERSION {
            return Err(BsrbDecodeError::Version(version));
        }
        let context = match cursor.u8()? {
            CTX_KIND_HASH => BsrbContext::Hash(H256::from_slice(cursor.take(32)?)),
            CTX_KIND_NUMBER => BsrbContext::Number(cursor.u64()?),
            other => return Err(BsrbDecodeError::ContextKind(other)),
        };
        let count = cursor.u16()? as usize;
        if count == 0 || count > BLOCKX_STATE_READ_BATCH_MAX_ITEMS {
            return Err(BsrbDecodeError::ItemCount(count));
        }
        let mut reads = Vec::with_capacity(count);
        for item in 0..count {
            let read = match cursor.u8()? {
                READ_KIND_ADDRESS_CODE => BsrbRead::AddressCode {
                    address: Address::from_slice(cursor.take(20)?),
                },
                READ_KIND_STORAGE_AT => BsrbRead::StorageAt {
                    address: Address::from_slice(cursor.take(20)?),
                    slot: H256::from_slice(cursor.take(32)?),
                },
                other => return Err(BsrbDecodeError::ReadKind(other, item)),
            };
            reads.push(read);
        }
        cursor.finish()?;
        Ok(Self { context, reads })
    }
}

impl BsrbResponse {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(3 + self.results.len() * 40);
        out.push(BSRB_VERSION);
        out.extend_from_slice(&(self.results.len() as u16).to_be_bytes());
        for outcome in &self.results {
            match outcome {
                BsrbOutcome::Value(value) => {
                    out.push(TAG_OK);
                    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
                    out.extend_from_slice(value);
                }
                BsrbOutcome::Error { code, message } => {
                    out.push(TAG_ERROR);
                    out.extend_from_slice(&code.to_be_bytes());
                    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
                    out.extend_from_slice(message.as_bytes());
                }
            }
        }
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self, BsrbDecodeError> {
        let mut cursor = Cursor::new(data);
        let version = cursor.u8()?;
        if version != BSRB_VERSION {
            return Err(BsrbDecodeError::Version(version));
        }
        let count = cursor.u16()? as usize;
        if count == 0 || count > BLOCKX_STATE_READ_BATCH_MAX_ITEMS {
            return Err(BsrbDecodeError::ItemCount(count));
        }
        let mut results = Vec::with_capacity(count);
        for item in 0..count {
            let outcome = match cursor.u8()? {
                TAG_OK => {
                    let len = cursor.u32()? as usize;
                    BsrbOutcome::Value(Bytes::copy_from_slice(cursor.take(len)?))
                }
                TAG_ERROR => {
                    let code = cursor.i32()?;
                    let len = cursor.u32()? as usize;
                    let message = String::from_utf8(cursor.take(len)?.to_vec())
                        .map_err(|_| BsrbDecodeError::MessageUtf8(item))?;
                    BsrbOutcome::Error { code, message }
                }
                other => return Err(BsrbDecodeError::OutcomeTag(other, item)),
            };
            results.push(outcome);
        }
        cursor.finish()?;
        Ok(Self { results })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> BsrbRequest {
        BsrbRequest {
            context: BsrbContext::Number(2),
            reads: vec![
                BsrbRead::AddressCode {
                    address: Address::repeat_byte(0x11),
                },
                BsrbRead::StorageAt {
                    address: Address::repeat_byte(0x22),
                    slot: H256::with_last_byte(0xfe),
                },
            ],
        }
    }

    #[test]
    fn request_round_trips() {
        let req = request();
        assert_eq!(BsrbRequest::decode(&req.encode()).unwrap(), req);
    }

    #[test]
    fn response_round_trips() {
        let resp = BsrbResponse {
            results: vec![
                BsrbOutcome::Value(Bytes::from(vec![0x60, 0x80])),
                BsrbOutcome::Value(Bytes::new()),
                BsrbOutcome::Error {
                    code: -39006,
                    message: "block 0x02 not found for state node".to_string(),
                },
            ],
        };
        assert_eq!(BsrbResponse::decode(&resp.encode()).unwrap(), resp);
    }

    #[test]
    fn strict_decoding_rejects_malformed_payloads() {
        let good = request().encode();

        let mut bad_version = good.clone();
        bad_version[0] = 2;
        assert_eq!(
            BsrbRequest::decode(&bad_version),
            Err(BsrbDecodeError::Version(2))
        );

        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(
            BsrbRequest::decode(&trailing),
            Err(BsrbDecodeError::TrailingBytes(1))
        );

        let truncated = &good[..good.len() - 1];
        assert!(matches!(
            BsrbRequest::decode(truncated),
            Err(BsrbDecodeError::Truncated { .. })
        ));

        let empty = BsrbRequest {
            context: BsrbContext::Number(1),
            reads: vec![],
        };
        assert_eq!(
            BsrbRequest::decode(&empty.encode()),
            Err(BsrbDecodeError::ItemCount(0))
        );

        let oversized = BsrbRequest {
            context: BsrbContext::Number(1),
            reads: vec![
                BsrbRead::AddressCode {
                    address: Address::ZERO
                };
                BLOCKX_STATE_READ_BATCH_MAX_ITEMS + 1
            ],
        };
        assert_eq!(
            BsrbRequest::decode(&oversized.encode()),
            Err(BsrbDecodeError::ItemCount(
                BLOCKX_STATE_READ_BATCH_MAX_ITEMS + 1
            ))
        );
    }
}
