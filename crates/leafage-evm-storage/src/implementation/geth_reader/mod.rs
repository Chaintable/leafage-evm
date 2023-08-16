use alloy_rlp::Decodable;
use alloy_rlp_derive::{RlpDecodable, RlpEncodable};
use leafage_evm_types::{Bytes, NewAccount, H160, H256, U256};
use leveldb_rs::{LevelDBError as RawError, DB};
use std::path::Path;

pub struct GethReader {
    db: DB,
}

#[derive(RlpEncodable, RlpDecodable, Clone, Debug)]
struct GethAccount {
    nonce: u64,
    balance: U256,
    root: Vec<u8>,
    code_hash: Vec<u8>,
}

impl GethReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Self {
        let db = DB::open(path.as_ref()).unwrap();
        GethReader { db }
    }

    pub fn account_scan<
        F1: FnMut(H160, NewAccount) -> bool,
        F2: FnMut(H160, U256, U256) -> bool,
    >(
        &mut self,
        mut f1: F1,
        mut f2: F2,
    ) -> Result<(), RawError> {
        let mut account_iter = self.db.iter()?.alloc();
        let mut storage_iter = self.db.iter()?.alloc();
        account_iter.seek(b"a");
        while let Some((key, val)) = account_iter.next() {
            if key.starts_with(b"a") {
                let mut bytes = val.as_ref();
                let account = GethAccount::decode(&mut bytes).unwrap();
                let address = H160::from_slice(&key[1..]);
                let account = NewAccount {
                    address: H160::from_slice(&key[1..]),
                    nonce: account.nonce,
                    balance: account.balance,
                    code_hash: H256::from_slice(&account.code_hash),
                    code: Bytes::default(),
                };
                let is_continue = f1(address, account);
                if !is_continue {
                    return Ok(());
                }
                let storage_key = [b"o", &key[1..]].concat();
                storage_iter.seek(&storage_key);
                while let Some((key, val)) = storage_iter.next() {
                    if key.starts_with(&storage_key) {
                        let storage_key = H256::from_slice(&key[storage_key.len()..]);
                        let storage_index = U256::from_be_bytes(storage_key.as_ref().0);
                        let val: Vec<u8> = Decodable::decode(&mut val.as_ref()).unwrap();
                        let storage_val = H256::from_slice(&val);
                        let storage_val = U256::from_be_bytes(storage_val.as_ref().0);
                        let is_continue = f2(address, storage_index, storage_val);
                        if !is_continue {
                            return Ok(());
                        }
                    } else {
                        break;
                    }
                }
            } else {
                break;
            }
        }
        Ok(())
    }

    pub fn code_scan<F: FnMut(H256, Bytes) -> bool>(&mut self, mut f: F) -> Result<(), RawError> {
        let mut iter = self.db.iter()?.alloc();
        iter.seek(b"c");
        while let Some((key, val)) = iter.next() {
            if key.starts_with(b"c") {
                let address = H256::from_slice(&key[1..]);
                let code = val.into();
                let is_continue = f(address, code);
                if !is_continue {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(())
    }
}
