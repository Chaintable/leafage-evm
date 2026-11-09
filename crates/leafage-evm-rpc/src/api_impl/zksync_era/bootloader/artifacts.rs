use primitive_types::U256;

pub const FEE_ESTIMATE_BYTECODE: &[u8] = include_bytes!("artifacts/fee_estimate.yul.zbin");
pub const DEFAULT_AA_BYTECODE: &[u8] = include_bytes!("artifacts/default_aa.bin");

pub const DEFAULT_AA_CODE_HASH: [u8; 32] = [
    0x01, 0x00, 0x05, 0x5b, 0x7a, 0x8b, 0xe9, 0x05, 0x22, 0x25, 0x1b, 0xe8, 0xbe, 0x1a, 0x18,
    0x64, 0x64, 0xd0, 0x56, 0x46, 0x29, 0x73, 0x50, 0x2a, 0xc8, 0xa0, 0x43, 0x7c, 0x85, 0xe4,
    0xd2, 0xa9,
];

pub const FEE_ESTIMATE_CODE_HASH: [u8; 32] = [
    0x01, 0x01, 0x09, 0x51, 0x96, 0x8c, 0x70, 0x1c, 0x02, 0x71, 0x47, 0x79, 0x29, 0x97, 0x12,
    0xd9, 0xda, 0x6e, 0x40, 0x0e, 0x56, 0xc7, 0x8d, 0x0d, 0x07, 0xac, 0xd9, 0x84, 0xbf, 0xe7,
    0x24, 0x2a,
];

pub fn default_aa_hash_u256() -> U256 {
    U256::from_big_endian(&DEFAULT_AA_CODE_HASH)
}

pub fn fee_estimate_hash_u256() -> U256 {
    U256::from_big_endian(&FEE_ESTIMATE_CODE_HASH)
}

pub fn compute_bytecode_hash(code: &[u8], is_constructor: bool) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    // zkSync Era bytecode hash is computed using SHA256:
    // format: [version (1 byte), is_constructor (1 byte), length_in_words (2 bytes), sha256(code)[4..32]]
    let words = (code.len() / 32) as u16;
    let mut hasher = Sha256::new();
    hasher.update(code);
    let digest = hasher.finalize();

    let mut out = [0u8; 32];
    out[0] = 1; // version 1
    out[1] = if is_constructor { 1 } else { 0 };
    out[2..4].copy_from_slice(&words.to_be_bytes());
    out[4..32].copy_from_slice(&digest[4..32]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bytecode_hashes() {
        let computed_aa = compute_bytecode_hash(DEFAULT_AA_BYTECODE, false);
        assert_eq!(computed_aa, DEFAULT_AA_CODE_HASH);

        let computed_bootloader = compute_bytecode_hash(FEE_ESTIMATE_BYTECODE, true);
        assert_eq!(computed_bootloader, FEE_ESTIMATE_CODE_HASH);
    }
}
