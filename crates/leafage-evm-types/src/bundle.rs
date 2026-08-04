use crate::error::{BundleStorageDiffError, BundleStorageDiffResult};
use crate::BlockStorageDiff;
use alloy_rlp::{decode_exact, Encodable};

const STATE_DIFF_ENTRY_CAPACITY: usize = 1_000;
const STATE_DIFF_BITMAP_BYTES: usize = 125;
const STATE_DIFF_OFFSET_COUNT: usize = STATE_DIFF_ENTRY_CAPACITY + 1;
const STATE_DIFF_OFFSET_BYTES: usize = 8;
const STATE_DIFF_INDEX_BYTES: usize =
    STATE_DIFF_BITMAP_BYTES + STATE_DIFF_OFFSET_COUNT * STATE_DIFF_OFFSET_BYTES;

pub struct BundleStorageDiff {
    index: BundleStorageDiffIndex,
    entries: [BlockStorageDiff; STATE_DIFF_ENTRY_CAPACITY],
}

impl BundleStorageDiff {
    pub fn encode(&self) -> BundleStorageDiffResult<Vec<u8>> {
        let payload_size = self.index.payload_size()?;
        let mut encoded = Vec::with_capacity(STATE_DIFF_INDEX_BYTES + payload_size);
        encoded.extend_from_slice(&self.index.encode()?);
        for entry in &self.entries {
            entry.encode(&mut encoded);
        }
        Ok(encoded)
    }

    pub fn decode(bytes: &[u8]) -> BundleStorageDiffResult<Self> {
        if bytes.len() < STATE_DIFF_INDEX_BYTES {
            return Err(BundleStorageDiffError::BundleTooShort {
                index_size: STATE_DIFF_INDEX_BYTES,
                actual: bytes.len(),
            });
        }

        let index = BundleStorageDiffIndex::decode(&bytes[..STATE_DIFF_INDEX_BYTES])?;
        let payload_size = index.payload_size()?;
        let actual_payload_size = bytes[STATE_DIFF_INDEX_BYTES..].len();
        if actual_payload_size != payload_size {
            return Err(BundleStorageDiffError::PayloadLengthMismatch {
                expected: payload_size,
                actual: actual_payload_size,
            });
        }

        let payload = &bytes[STATE_DIFF_INDEX_BYTES..];
        let mut entries = Vec::with_capacity(STATE_DIFF_ENTRY_CAPACITY);
        for position in 0..STATE_DIFF_ENTRY_CAPACITY {
            let start = index.offset_as_usize(position)?;
            let end = index.offset_as_usize(position + 1)?;
            let entry = decode_exact(&payload[start..end])
                .map_err(|source| BundleStorageDiffError::Rlp { position, source })?;
            entries.push(entry);
        }

        Ok(Self {
            index,
            entries: entries
                .try_into()
                .expect("1,001 offsets always produce 1,000 entries"),
        })
    }
}

pub struct BundleStorageDiffIndex {
    bitmap: [u8; STATE_DIFF_BITMAP_BYTES],
    offset: [u64; STATE_DIFF_OFFSET_COUNT],
}

impl BundleStorageDiffIndex {
    pub fn payload_size(&self) -> BundleStorageDiffResult<usize> {
        self.offset_as_usize(STATE_DIFF_ENTRY_CAPACITY)
    }

    pub fn encode(&self) -> BundleStorageDiffResult<Vec<u8>> {
        self.validate()?;

        let mut encoded = Vec::with_capacity(STATE_DIFF_INDEX_BYTES);
        encoded.extend_from_slice(&self.bitmap);
        for offset in self.offset {
            encoded.extend_from_slice(&offset.to_be_bytes());
        }
        Ok(encoded)
    }

    pub fn decode(bytes: &[u8]) -> BundleStorageDiffResult<Self> {
        if bytes.len() != STATE_DIFF_INDEX_BYTES {
            return Err(BundleStorageDiffError::InvalidIndexLength {
                expected: STATE_DIFF_INDEX_BYTES,
                actual: bytes.len(),
            });
        }

        let mut bitmap = [0_u8; STATE_DIFF_BITMAP_BYTES];
        bitmap.copy_from_slice(&bytes[..STATE_DIFF_BITMAP_BYTES]);

        let mut offset = [0_u64; STATE_DIFF_OFFSET_COUNT];
        for (position, chunk) in bytes[STATE_DIFF_BITMAP_BYTES..]
            .chunks_exact(STATE_DIFF_OFFSET_BYTES)
            .enumerate()
        {
            offset[position] = u64::from_be_bytes(
                chunk
                    .try_into()
                    .expect("chunks_exact always yields eight-byte chunks"),
            );
        }

        let index = Self { bitmap, offset };
        index.validate()?;
        Ok(index)
    }

    fn validate(&self) -> BundleStorageDiffResult<()> {
        if self.offset[0] != 0 {
            return Err(BundleStorageDiffError::NonZeroFirstOffset {
                actual: self.offset[0],
            });
        }

        for (position, window) in self.offset.windows(2).enumerate() {
            let current = window[0];
            let next = window[1];
            if next <= current {
                return Err(BundleStorageDiffError::NonIncreasingOffset {
                    position,
                    current,
                    next,
                });
            }
        }

        // The bitmap marks real versus synthesized StateDiffs. Both have an RLP
        // payload, so bitmap bits do not change offset validation.
        self.payload_size()?;
        Ok(())
    }

    fn offset_as_usize(&self, position: usize) -> BundleStorageDiffResult<usize> {
        let offset = self.offset[position];
        usize::try_from(offset)
            .map_err(|_| BundleStorageDiffError::OffsetOverflow { position, offset })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_offsets() -> [u64; STATE_DIFF_OFFSET_COUNT] {
        let mut offsets = [0; STATE_DIFF_OFFSET_COUNT];
        for position in 1..STATE_DIFF_OFFSET_COUNT {
            offsets[position] = offsets[position - 1] + 100;
        }
        offsets
    }

    fn index() -> BundleStorageDiffIndex {
        BundleStorageDiffIndex {
            bitmap: [0xff; STATE_DIFF_BITMAP_BYTES],
            offset: new_offsets(),
        }
    }

    fn write_offset(bytes: &mut [u8], position: usize, value: u64) {
        let start = STATE_DIFF_BITMAP_BYTES + position * STATE_DIFF_OFFSET_BYTES;
        bytes[start..start + STATE_DIFF_OFFSET_BYTES].copy_from_slice(&value.to_be_bytes());
    }

    #[test]
    fn index_round_trips() {
        let expected = index();
        let encoded = expected.encode().unwrap();
        let decoded = BundleStorageDiffIndex::decode(&encoded).unwrap();

        assert_eq!(encoded.len(), STATE_DIFF_INDEX_BYTES);
        assert_eq!(decoded.bitmap, expected.bitmap);
        assert_eq!(decoded.offset, expected.offset);
    }

    #[test]
    fn index_rejects_wrong_length() {
        let encoded = index().encode().unwrap();

        assert!(matches!(
            BundleStorageDiffIndex::decode(&encoded[..encoded.len() - 1]),
            Err(BundleStorageDiffError::InvalidIndexLength { .. })
        ));
    }

    #[test]
    fn index_rejects_nonzero_first_offset() {
        let mut encoded = index().encode().unwrap();
        write_offset(&mut encoded, 0, 1);

        assert!(matches!(
            BundleStorageDiffIndex::decode(&encoded),
            Err(BundleStorageDiffError::NonZeroFirstOffset { actual: 1 })
        ));
    }

    #[test]
    fn index_rejects_non_increasing_offsets() {
        let mut encoded = index().encode().unwrap();
        write_offset(&mut encoded, 2, 100);

        assert!(matches!(
            BundleStorageDiffIndex::decode(&encoded),
            Err(BundleStorageDiffError::NonIncreasingOffset {
                position: 1,
                current: 100,
                next: 100
            })
        ));
    }

    #[test]
    fn bundle_rejects_short_index_and_payload_length_mismatch() {
        assert!(matches!(
            BundleStorageDiff::decode(&vec![0; STATE_DIFF_INDEX_BYTES - 1]),
            Err(BundleStorageDiffError::BundleTooShort { .. })
        ));

        let encoded_index = index().encode().unwrap();
        assert!(matches!(
            BundleStorageDiff::decode(&encoded_index),
            Err(BundleStorageDiffError::PayloadLengthMismatch {
                expected: 100_000,
                actual: 0
            })
        ));
    }

    #[test]
    fn bundle_round_trips_with_payload_relative_offsets() {
        let entries = std::array::from_fn(|_| BlockStorageDiff::default());
        let entry_size = entries[0].length() as u64;
        let mut offset = [0; STATE_DIFF_OFFSET_COUNT];
        for position in 1..STATE_DIFF_OFFSET_COUNT {
            offset[position] = offset[position - 1] + entry_size;
        }
        let bundle = BundleStorageDiff {
            index: BundleStorageDiffIndex {
                bitmap: [0xff; STATE_DIFF_BITMAP_BYTES],
                offset,
            },
            entries,
        };

        let encoded = bundle.encode().unwrap();
        let decoded = BundleStorageDiff::decode(&encoded).unwrap();

        assert_eq!(decoded.index.bitmap, bundle.index.bitmap);
        assert_eq!(decoded.index.offset, bundle.index.offset);
        assert_eq!(decoded.entries, bundle.entries);

        let mut invalid_rlp = encoded.clone();
        invalid_rlp[STATE_DIFF_INDEX_BYTES] = 0xff;
        assert!(matches!(
            BundleStorageDiff::decode(&invalid_rlp),
            Err(BundleStorageDiffError::Rlp { position: 0, .. })
        ));

        let mut trailing_rlp = encoded;
        trailing_rlp.push(0x80);
        write_offset(
            &mut trailing_rlp,
            STATE_DIFF_ENTRY_CAPACITY,
            bundle.index.offset[STATE_DIFF_ENTRY_CAPACITY] + 1,
        );
        assert!(matches!(
            BundleStorageDiff::decode(&trailing_rlp),
            Err(BundleStorageDiffError::Rlp { position: 999, .. })
        ));
    }
}
