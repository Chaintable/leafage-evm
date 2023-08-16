use crate::scheme::{SlimAccount, StateDBRead, StateDBWrite};
use alloy_rlp::{Decodable, Encodable, Error};
use leafage_evm_types::{BlockInfo, Bytes, NewAccount, H160, H256, U256};
use leveldb_rs::{DBWriteBatch, LevelDBError as RawError, DB};
use std::path::Path;
use std::sync::{Mutex, RwLock};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum LevelDBError {
    #[error("leveldb error: {0}")]
    DBRaw(RawError),
    #[error("db error: {0}")]
    Other(String),
    #[error("rlp decode error: {0}")]
    RlpDecode(#[from] Error),
}

pub struct DataBase {
    db: RwLock<DB>,
    write_batch: Mutex<Option<DBWriteBatch>>,
}

unsafe impl Send for DataBase {}

unsafe impl Sync for DataBase {}

impl DataBase {
    pub fn open<P: AsRef<Path>>(path: P) -> Self {
        let db = DB::create(path.as_ref()).unwrap();
        let db = RwLock::new(db);
        let write_batch = Mutex::new(None);
        DataBase { db, write_batch }
    }
}

#[derive(Debug)]
enum StorageTypePrefix {
    LatestBlockHash,
    // block hash -> block info
    BlockHashToBlockInfo,
    // block num -> block hash
    BlockNumToBlockHash,
    // address -> account
    AddressToAccount,
    // address || storage index -> storage
    AddressToStorage,
    // code hash -> code
    HashToCode,
}

impl StorageTypePrefix {
    fn to_str(&self) -> &'static str {
        match self {
            StorageTypePrefix::LatestBlockHash => "LastBlockInfo",
            StorageTypePrefix::BlockHashToBlockInfo => "hti-",
            StorageTypePrefix::BlockNumToBlockHash => "nti-",
            StorageTypePrefix::AddressToAccount => "a",
            StorageTypePrefix::AddressToStorage => "s",
            StorageTypePrefix::HashToCode => "c",
        }
    }
}

impl StateDBRead for DataBase {
    type Error = LevelDBError;
    fn read_latest_block_hash(&self) -> Result<H256, Self::Error> {
        let key = StorageTypePrefix::LatestBlockHash.to_str().as_bytes();
        match self.db.read().unwrap().get(key) {
            Ok(Some(value)) => Ok(H256::from_slice(&value)),
            Ok(None) => Err(LevelDBError::Other(
                "latest block hash not found".to_string(),
            )),
            Err(e) => Err(LevelDBError::DBRaw(e)),
        }
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<BlockInfo>, Self::Error> {
        let key = StorageTypePrefix::BlockHashToBlockInfo.to_str().as_bytes();
        let key = [key, block_hash.as_ref().as_bytes()].concat();
        match self.db.read().unwrap().get(&key) {
            Ok(Some(value)) => {
                let mut bytes = value.as_ref();
                let block_info = BlockInfo::decode(&mut bytes)?;
                Ok(Some(block_info))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(LevelDBError::DBRaw(e)),
        }
    }

    fn read_block_num(&self, block_num: U256) -> Result<H256, Self::Error> {
        let key = StorageTypePrefix::BlockNumToBlockHash.to_str().as_bytes();
        let key = [key, block_num.to_be_bytes_vec().as_ref()].concat();
        match self.db.read().unwrap().get(&key) {
            Ok(Some(value)) => Ok(H256::from_slice(&value)),
            Ok(None) => Err(LevelDBError::Other("block num not found".to_string())),
            Err(e) => Err(LevelDBError::DBRaw(e)),
        }
    }

    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, Self::Error> {
        let key = StorageTypePrefix::HashToCode.to_str().as_bytes();
        let key = [key, code_hash.as_ref().as_bytes()].concat();
        match self.db.read().unwrap().get(&key) {
            Ok(Some(value)) => Ok(Some(value.into())),
            Ok(None) => Ok(None),
            Err(e) => Err(LevelDBError::DBRaw(e)),
        }
    }

    fn read_account(&self, address: H160) -> Result<Option<NewAccount>, Self::Error> {
        let key = StorageTypePrefix::AddressToAccount.to_str().as_bytes();
        let key = [key, address.as_ref().as_bytes()].concat();
        match self.db.read().unwrap().get(&key) {
            Ok(Some(value)) => {
                let mut bytes = value.as_ref();
                let account = SlimAccount::decode(&mut bytes)?;
                let account = NewAccount {
                    address,
                    balance: account.balance,
                    nonce: account.nonce,
                    code_hash: account.code_hash,
                    code: Bytes::default(),
                };
                Ok(Some(account))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(LevelDBError::DBRaw(e)),
        }
    }

    fn read_storage(&self, address: H160, index: U256) -> Result<U256, Self::Error> {
        let key = StorageTypePrefix::AddressToStorage.to_str().as_bytes();
        let key = [
            key,
            address.as_ref().as_bytes(),
            index.to_be_bytes_trimmed_vec().as_ref(),
        ]
        .concat();
        match self.db.read().unwrap().get(&key) {
            Ok(Some(value)) => {
                let mut value_array: [u8; U256::BYTES] = [0; U256::BYTES];
                assert!(value.len() <= U256::BYTES);
                value_array[U256::BYTES - value.len()..].copy_from_slice(&value);
                let value = U256::from_be_bytes(value_array);
                Ok(value)
            }
            Ok(None) => Ok(U256::ZERO),
            Err(e) => Err(LevelDBError::DBRaw(e)),
        }
    }
}

impl StateDBWrite for DataBase {
    type Error = LevelDBError;
    fn prepare_write_batch(&self) -> Result<(), Self::Error> {
        *self.write_batch.lock().unwrap() = Some(DBWriteBatch::new().unwrap());
        Ok(())
    }

    fn write_latest_block_hash(&self, block_hash: H256) -> Result<(), Self::Error> {
        let key = StorageTypePrefix::LatestBlockHash.to_str().as_bytes();
        let value = block_hash.as_ref().as_bytes();
        self.write_batch
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .put(key, value);
        Ok(())
    }

    fn write_block_num(&self, block_num: U256, block_hash: H256) -> Result<(), Self::Error> {
        let key = StorageTypePrefix::BlockNumToBlockHash.to_str().as_bytes();
        let key = [key, block_num.to_be_bytes_vec().as_ref()].concat();
        let value = block_hash.as_ref().as_bytes();
        self.write_batch
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .put(&key, value);
        Ok(())
    }

    fn write_block_info(&self, block_info: BlockInfo) -> Result<(), Self::Error> {
        let key = StorageTypePrefix::BlockHashToBlockInfo.to_str().as_bytes();
        let key = [key, block_info.hash.as_ref().as_bytes()].concat();
        let mut value = Vec::new();
        block_info.encode(&mut value);
        self.write_batch
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .put(&key, &value);
        Ok(())
    }

    fn write_code(&self, code_hash: H256, code: Bytes) -> Result<(), Self::Error> {
        let key = StorageTypePrefix::HashToCode.to_str().as_bytes();
        let key = [key, code_hash.as_ref().as_bytes()].concat();
        self.write_batch
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .put(&key, code.as_ref());
        Ok(())
    }

    fn write_account(
        &self,
        address: H160,
        raw_account: Option<NewAccount>,
    ) -> Result<(), Self::Error> {
        let key = StorageTypePrefix::AddressToAccount.to_str().as_bytes();
        let key = [key, address.as_ref().as_bytes()].concat();
        if let Some(account) = raw_account {
            let account: SlimAccount = account.into();
            let mut value = Vec::new();
            account.encode(&mut value);
            self.write_batch
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .put(&key, &value);
        } else {
            self.write_batch
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .delete(&key);
        }
        Ok(())
    }

    fn write_storage(&self, address: H160, index: U256, value: U256) -> Result<(), Self::Error> {
        let key = StorageTypePrefix::AddressToStorage.to_str().as_bytes();
        let key = [
            key,
            address.as_ref().as_bytes(),
            index.to_be_bytes_trimmed_vec().as_ref(),
        ]
        .concat();
        let value = value.to_be_bytes_trimmed_vec();
        self.write_batch
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .put(&key, &value);
        Ok(())
    }

    fn commit(&self) -> Result<(), Self::Error> {
        if let Some(batch) = self.write_batch.lock().unwrap().take() {
            match self.db.write().unwrap().write(batch) {
                Ok(_) => Ok(()),
                Err(e) => Err(LevelDBError::DBRaw(e)),
            }
        } else {
            Ok(())
        }
    }
}
