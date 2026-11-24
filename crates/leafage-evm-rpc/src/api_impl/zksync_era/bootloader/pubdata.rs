use primitive_types::U256;
use super::constants::{TX_MEMORY_OVERHEAD_GAS, TX_SLOT_OVERHEAD_GAS};

/// Returns how many slots (32-byte words) ABI-encoding of the transaction with such parameters takes.
pub fn encoding_len(
    data_len: u64,
    signature_len: u64,
    factory_deps_len: u64,
    paymaster_input_len: u64,
    reserved_dynamic_len: u64,
) -> usize {
    // 1 (offset to tuple) + 19 (tuple head) + 5 (dynamic field lengths)
    const BASE_LEN: usize = 1 + 19 + 5;

    let dynamic_len = data_len.div_ceil(32)
        + signature_len.div_ceil(32)
        + paymaster_input_len.div_ceil(32)
        + reserved_dynamic_len.div_ceil(32)
        + factory_deps_len;

    BASE_LEN + dynamic_len as usize
}

/// Derives the operator overhead for a transaction given its ABI encoding length in words.
pub fn derive_overhead(encoded_len: usize) -> u32 {
    TX_SLOT_OVERHEAD_GAS.max(TX_MEMORY_OVERHEAD_GAS * (encoded_len as u32))
}

fn metadata_byte(output_size: usize, operation_id: usize) -> u8 {
    ((output_size << 3) | operation_id) as u8
}

fn compress_extended(diff: U256, size: usize, op_id: usize) -> Vec<u8> {
    let mut buffer = [0u8; 32];
    diff.to_big_endian(&mut buffer);
    let mut res = Vec::with_capacity(1 + size);
    res.push(metadata_byte(size, op_id));
    res.extend_from_slice(&buffer[(32 - size)..]);
    res
}

/// Compresses storage values using the most efficient compression strategy matching zkSync Era protocol.
pub fn compress_with_best_strategy(prev_value: U256, new_value: U256) -> Vec<u8> {
    let mut candidates: Vec<Vec<u8>> = Vec::with_capacity(4);

    // Op 1: Addition
    let add_diff = new_value.overflowing_sub(prev_value).0;
    let add_size = add_diff.bits().div_ceil(8);
    if add_size < 31 {
        candidates.push(compress_extended(add_diff, add_size, 1));
    }

    // Op 2: Subtraction
    let sub_diff = prev_value.overflowing_sub(new_value).0;
    let sub_size = sub_diff.bits().div_ceil(8);
    if sub_size < 31 {
        candidates.push(compress_extended(sub_diff, sub_size, 2));
    }

    // Op 3: Transform (new value only)
    let transform_size = new_value.bits().div_ceil(8);
    if transform_size < 31 {
        candidates.push(compress_extended(new_value, transform_size, 3));
    }

    // Default Op 0: None (uncompressed 32 bytes)
    candidates
        .into_iter()
        .min_by_key(|c| c.len())
        .unwrap_or_else(|| {
            let mut res = vec![0u8; 33];
            new_value.to_big_endian(&mut res[1..33]);
            res
        })
}

/// Computes the published pubdata byte cost for a storage write.
pub fn cost_of_writing_storage(slot_value: U256, is_write_initial: bool, new_value: U256) -> u32 {
    if slot_value == new_value {
        return 0;
    }

    let compressed_value_size = compress_with_best_strategy(slot_value, new_value).len() as u32;

    if is_write_initial {
        // BYTES_PER_DERIVED_KEY (32) + compressed value size
        32 + compressed_value_size
    } else {
        // BYTES_PER_ENUMERATION_INDEX (4) + compressed value size
        4 + compressed_value_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encoding_len() {
        assert_eq!(encoding_len(0, 0, 0, 0, 0), 25);
        assert_eq!(encoding_len(65, 75, 2, 85, 32), 37);
    }

    #[test]
    fn test_derive_overhead() {
        assert_eq!(derive_overhead(25), 10_000);
        assert_eq!(derive_overhead(2000), 20_000);
    }

    #[test]
    fn test_compress_with_best_strategy() {
        let initial_val = U256::from(255438218);
        let final_val = U256::from(255438638);
        let compressed = compress_with_best_strategy(initial_val, final_val);
        // Diff is 420 (fits in 2 bytes), op = 1 (add)
        assert_eq!(compressed.len(), 3); // 1 byte metadata + 2 bytes diff
        assert_eq!(compressed[0] & 0x07, 1);
    }
}
