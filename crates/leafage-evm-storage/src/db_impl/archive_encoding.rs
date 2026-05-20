//! Shared encoding helpers for archive storage keys/values.
//!
//! Both MDBX and RocksDB archive backends use the same on-disk encoding,
//! so callers (e.g. bulk archive ingest) can pre-compute encoded entries
//! off the writer thread and feed them directly into a batch.

use alloy_rlp::Encodable;
use leafage_evm_types::{NewAccount, SlimAccount, H256};

/// Length of the encoded `(address, block_num)` account key.
pub const ACCOUNT_KEY_LEN: usize = 64;
/// Length of the encoded `(address, storage_key, block_num)` storage key.
pub const STORAGE_KEY_LEN: usize = 96;

#[inline]
pub fn encode_block_num(block_num: u64) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[24..32].copy_from_slice(&block_num.to_be_bytes());
    bytes
}

/// Encode account key: `address(32) || block_num(32 BE)`.
#[inline]
pub fn encode_account_key(address: H256, block_num: u64) -> [u8; ACCOUNT_KEY_LEN] {
    let mut key = [0u8; ACCOUNT_KEY_LEN];
    key[..32].copy_from_slice(address.as_slice());
    key[32..64].copy_from_slice(&encode_block_num(block_num));
    key
}

/// Encode storage key: `address(32) || storage_key(32) || block_num(32 BE)`.
#[inline]
pub fn encode_storage_key(
    address: H256,
    storage_key: H256,
    block_num: u64,
) -> [u8; STORAGE_KEY_LEN] {
    let mut key = [0u8; STORAGE_KEY_LEN];
    key[..32].copy_from_slice(address.as_slice());
    key[32..64].copy_from_slice(storage_key.as_slice());
    key[64..96].copy_from_slice(&encode_block_num(block_num));
    key
}

/// RLP-encode the slim form of an account (balance, nonce, code_hash) used
/// as the value for `AddressToAccount`.
#[inline]
pub fn encode_slim_account(account: NewAccount) -> Vec<u8> {
    let slim: SlimAccount = account.into();
    let mut buf = Vec::new();
    slim.encode(&mut buf);
    buf
}
