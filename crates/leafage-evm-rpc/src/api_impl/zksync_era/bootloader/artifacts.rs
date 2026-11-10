use primitive_types::U256;

pub const FEE_ESTIMATE_BYTECODE: &[u8] = include_bytes!("artifacts/fee_estimate.yul.zbin");
pub const PLAYGROUND_BATCH_BYTECODE: &[u8] = include_bytes!("artifacts/playground_batch.yul.zbin");
pub const DEFAULT_AA_BYTECODE: &[u8] = include_bytes!("artifacts/default_aa.bin");
pub const EVM_EMULATOR_BYTECODE: &[u8] = include_bytes!("artifacts/evm_emulator.bin");

pub const DEFAULT_AA_CODE_HASH: [u8; 32] = [
    0x01, 0x00, 0x05, 0xd9, 0xae, 0x0f, 0xb8, 0xab, 0x4b, 0xc7, 0x3a, 0x1b, 0x99, 0x34, 0xc3,
    0x44, 0x13, 0x9a, 0x2e, 0xa0, 0x28, 0x94, 0x39, 0x4f, 0xe7, 0x96, 0x33, 0xcd, 0xd0, 0x46,
    0x3f, 0x29,
];

pub const EVM_EMULATOR_CODE_HASH: [u8; 32] = [
    0x01, 0x00, 0x0c, 0x07, 0xb3, 0xdb, 0x6a, 0xf3, 0xa6, 0x45, 0x75, 0x9f, 0x18, 0x1e, 0xe4,
    0x9d, 0xdb, 0xa1, 0xa8, 0x9e, 0x50, 0x17, 0x96, 0xde, 0x17, 0x6d, 0xde, 0x6b, 0x98, 0x0d,
    0x2a, 0x60,
];

pub const FEE_ESTIMATE_CODE_HASH: [u8; 32] = [
    0x01, 0x01, 0x09, 0x51, 0x96, 0x8c, 0x70, 0x1c, 0x02, 0x71, 0x47, 0x79, 0x29, 0x97, 0x12,
    0xd9, 0xda, 0x6e, 0x40, 0x0e, 0x56, 0xc7, 0x8d, 0x0d, 0x07, 0xac, 0xd9, 0x84, 0xbf, 0xe7,
    0x24, 0x2a,
];

pub const PLAYGROUND_BATCH_CODE_HASH: [u8; 32] = [
    0x01, 0x01, 0x0a, 0x31, 0xde, 0x8d, 0x26, 0x23, 0x29, 0x70, 0xdf, 0x6c, 0x93, 0xf0, 0x65,
    0x9d, 0xf5, 0xa9, 0x17, 0xb8, 0xc8, 0x9c, 0x18, 0x12, 0x39, 0x6b, 0xaa, 0x80, 0xfc, 0x44,
    0xe9, 0x46,
];

pub fn default_aa_hash_u256() -> U256 {
    U256::from_big_endian(&DEFAULT_AA_CODE_HASH)
}

pub fn evm_emulator_hash_u256() -> U256 {
    U256::from_big_endian(&EVM_EMULATOR_CODE_HASH)
}

pub fn fee_estimate_hash_u256() -> U256 {
    U256::from_big_endian(&FEE_ESTIMATE_CODE_HASH)
}

pub fn playground_batch_hash_u256() -> U256 {
    U256::from_big_endian(&PLAYGROUND_BATCH_CODE_HASH)
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

        let computed_playground = compute_bytecode_hash(PLAYGROUND_BATCH_BYTECODE, true);
        assert_eq!(computed_playground, PLAYGROUND_BATCH_CODE_HASH);

        let computed_evm_emulator = compute_bytecode_hash(EVM_EMULATOR_BYTECODE, false);
        assert_eq!(computed_evm_emulator, EVM_EMULATOR_CODE_HASH);
    }
}
