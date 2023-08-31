//! RocksDB implementation of the database.
//!
//! Data is stored in the following format:
//! ```text
//! +-------------------------+-------------------------+-------------------------+
//! |  LatestBlockHash        |  BlockHashToBlockInfo   |  BlockNumToBlockHash    |
//! +-------------------------+-------------------------+-------------------------+
//! |  AddressToAccount       |  AddressToStorage       |  HashToCode             |
//! +-------------------------+-------------------------+-------------------------+
//! ```
//! The `LatestBlockHash` column family stores the latest block hash.
//! The `BlockHashToBlockInfo` column family stores the block hash to block info maps.
//! The `BlockNumToBlockHash` column family stores the block number to block hash maps.
//! The `AddressToAccount` column family stores the address to account maps.
//! The `AddressToStorage` column family stores the (address,index) to storage maps.
//! The `HashToCode` column family stores the code hash to code maps.
//! All [`U256`] are big-endian encoded.

use crate::db::{StateDBRead, StateDBWrite};
use leafage_evm_types::{
    trim_left_zero_bytes, Block, Bytes, NewAccount, SlimAccount, Transaction, H256, KECCAK_EMPTY,
    U256,
};
use open_fastrlp::{Decodable, Encodable};
use rocksdb::{BlockBasedOptions, Cache, ColumnFamilyDescriptor, Options, WriteBatch, DB};
use serde_json::{from_slice, to_vec};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("rocksdb error, {0}")]
    RocksDB(#[from] rocksdb::Error),
    #[error("serde_json error, {0}")]
    SerdeJson(#[from] serde_json::Error),
}

#[repr(u16)]
#[derive(Debug)]
enum StorageTypeColumn {
    LatestBlockHash = 1,
    // block hash -> block info
    BlockHashToBlockInfo = 2,
    // block num -> block hash
    BlockNumToBlockHash = 3,
    // address -> account
    AddressToAccount = 4,
    // address || storage index -> storage
    AddressToStorage = 5,
    // code hash -> code
    HashToCode = 6,
}

impl StorageTypeColumn {
    fn to_str(&self) -> &'static str {
        match self {
            StorageTypeColumn::LatestBlockHash => "1",
            StorageTypeColumn::BlockHashToBlockInfo => "2",
            StorageTypeColumn::BlockNumToBlockHash => "3",
            StorageTypeColumn::AddressToAccount => "4",
            StorageTypeColumn::AddressToStorage => "5",
            StorageTypeColumn::HashToCode => "6",
        }
    }
}

pub struct DataBase {
    db: DB,
}

impl StateDBRead for DataBase {
    type Error = Error;
    fn read_block_hash(&self, block_num: U256) -> Result<H256, Error> {
        let block_num_to_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .unwrap();
        let block_num_bytes: [u8; 32] = block_num.into();
        let block_hash_bytes = self
            .db
            .get_cf(block_num_to_block_hash_cf, block_num_bytes)?;
        if block_hash_bytes.is_none() {
            return Ok(H256::zero());
        }
        let block_hash_bytes = block_hash_bytes.unwrap();
        let block_hash = H256::from_slice(block_hash_bytes.as_slice());
        Ok(block_hash)
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<Block<Transaction>>, Error> {
        let block_hash_to_block_info_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
            .unwrap();
        let block_hash_bytes = block_hash.as_bytes();
        let block_info_bytes = self
            .db
            .get_cf(block_hash_to_block_info_cf, block_hash_bytes)?;
        if block_info_bytes.is_none() {
            return Ok(None);
        }
        let block_info_bytes = block_info_bytes.unwrap();
        let block_info_slice = block_info_bytes.as_slice();
        let block_info = from_slice::<Block<Transaction>>(block_info_slice)?;
        Ok(Some(block_info))
    }

    fn read_account(&self, address: H256) -> Result<Option<NewAccount>, Error> {
        let address_to_account_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
            .unwrap();
        let address_bytes = address.as_bytes();
        let raw_account_bytes = self.db.get_cf(address_to_account_cf, address_bytes)?;
        if raw_account_bytes.is_none() {
            return Ok(None);
        }
        let raw_account_bytes = raw_account_bytes.unwrap();
        let mut raw_account_slice = raw_account_bytes.as_slice();
        let account = SlimAccount::decode(&mut raw_account_slice).unwrap();
        let account = NewAccount {
            address,
            balance: account.balance,
            nonce: account.nonce,
            code_hash: if account.code_hash.is_zero() {
                KECCAK_EMPTY.into()
            } else {
                account.code_hash
            },
        };
        Ok(Some(account))
    }

    fn read_storage(&self, address: H256, key: H256) -> Result<U256, Error> {
        let address_to_storage_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
            .unwrap();
        let address_bytes = address.as_bytes();
        let key_bytes: [u8; 32] = key.into();
        let value_bytes = self.db.get_cf(
            address_to_storage_cf,
            [address_bytes, trim_left_zero_bytes(&key_bytes)].concat(),
        )?;
        if value_bytes.is_none() {
            return Ok(U256::zero());
        }
        let value_bytes = value_bytes.unwrap();
        let value = U256::from_big_endian(value_bytes.as_slice());
        Ok(value)
    }

    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, Error> {
        let address_to_code_cf = self
            .db
            .cf_handle(StorageTypeColumn::HashToCode.to_str())
            .unwrap();
        let code_hash_bytes = code_hash.as_bytes();
        let code = self.db.get_cf(address_to_code_cf, code_hash_bytes)?;
        if code.is_none() {
            return Ok(None);
        }
        Ok(Some(Bytes::from(code.unwrap())))
    }

    fn read_latest_block_hash(&self) -> Result<H256, Error> {
        let latest_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
            .unwrap();
        let block_hash_bytes = self.db.get_cf(latest_block_hash_cf, [1u8].to_vec())?;
        if block_hash_bytes.is_none() {
            return Ok(H256::zero());
        }
        let block_hash_bytes = block_hash_bytes.unwrap();
        let block_hash = H256::from_slice(block_hash_bytes.as_slice());
        Ok(block_hash)
    }
}

impl StateDBWrite for DataBase {
    type Error = Error;
    type DBWriteBatch = WriteBatch;
    fn prepare_write_batch(&self) -> Result<WriteBatch, Self::Error> {
        Ok(WriteBatch::default())
    }
    fn write_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_num: U256,
        block_hash: H256,
    ) -> Result<(), Self::Error> {
        let block_num_to_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .unwrap();
        let block_hash_bytes = block_hash.as_bytes();
        let block_num_bytes: [u8; 32] = block_num.into();
        batch.put_cf(
            block_num_to_block_hash_cf,
            block_num_bytes,
            block_hash_bytes,
        );
        Ok(())
    }

    fn write_block_info(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_info: Block<Transaction>,
    ) -> Result<(), Error> {
        let block_hash_to_block_info_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
            .unwrap();

        let block_info_bytes = to_vec(&block_info)?;
        let block_hash = block_info.hash.unwrap();
        batch.put_cf(
            block_hash_to_block_info_cf,
            block_hash.as_bytes(),
            block_info_bytes,
        );
        Ok(())
    }

    fn write_account(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        raw_account: Option<NewAccount>,
    ) -> Result<(), Error> {
        let address_to_account_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
            .unwrap();
        let address_bytes = address.as_bytes();
        if let Some(raw_account) = raw_account {
            let raw_account: SlimAccount = raw_account.into();
            let mut raw_account_bytes = Vec::new();
            raw_account.encode(&mut raw_account_bytes);
            batch.put_cf(address_to_account_cf, address_bytes, raw_account_bytes);
        } else {
            batch.delete_cf(address_to_account_cf, address_bytes);
        }
        Ok(())
    }

    fn write_storage(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        key: H256,
        value: U256,
    ) -> Result<(), Error> {
        let address_to_storage_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
            .unwrap();
        let address_bytes = address.as_bytes();
        let key_bytes: [u8; 32] = key.into();
        if value == U256::zero() {
            batch.delete_cf(address_to_storage_cf, [address_bytes, &key_bytes].concat());
            return Ok(());
        } else {
            let value_bytes: [u8; 32] = value.into();
            batch.put_cf(
                address_to_storage_cf,
                [address_bytes, &key_bytes].concat(),
                value_bytes,
            );
        }
        Ok(())
    }

    fn write_code(
        &self,
        batch: &mut Self::DBWriteBatch,
        code_hash: H256,
        code: Bytes,
    ) -> Result<(), Error> {
        let address_to_code_cf = self
            .db
            .cf_handle(StorageTypeColumn::HashToCode.to_str())
            .unwrap();
        let code_hash_bytes = code_hash.as_bytes();
        batch.put_cf(address_to_code_cf, code_hash_bytes, code);
        Ok(())
    }

    fn write_latest_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_hash: H256,
    ) -> Result<(), Error> {
        let latest_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
            .unwrap();
        batch.put_cf(latest_block_hash_cf, [1u8].to_vec(), block_hash.as_bytes());
        Ok(())
    }

    fn commit(&self, batch: Self::DBWriteBatch) -> Result<(), Self::Error> {
        self.db.write(batch)?;
        Ok(())
    }
}

impl DataBase {
    pub fn open<P: AsRef<Path>>(path: P) -> Self {
        let mut cf_opts = Options::default();
        cf_opts.set_max_write_buffer_number(16);
        cf_opts.create_if_missing(true);
        let latest_block_hash_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::LatestBlockHash.to_str(),
            cf_opts.clone(),
        );
        let block_hash_to_block_info_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::BlockHashToBlockInfo.to_str(),
            cf_opts.clone(),
        );
        let block_num_to_block_hash_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::BlockNumToBlockHash.to_str(),
            cf_opts.clone(),
        );
        let address_to_account_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToAccount.to_str(),
            cf_opts.clone(),
        );
        let address_to_storage_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToStorage.to_str(),
            cf_opts.clone(),
        );
        let hash_to_code_cf =
            ColumnFamilyDescriptor::new(StorageTypeColumn::HashToCode.to_str(), cf_opts.clone());
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        db_opts.set_max_write_buffer_number(32);
        let mut block_opts = BlockBasedOptions::default();
        block_opts.set_bloom_filter(10.0, false);
        let cache = Cache::new_lru_cache(1 << 30); // e.g., 1GB
        block_opts.set_block_cache(&cache);
        db_opts.set_block_based_table_factory(&block_opts);
        db_opts.set_write_buffer_size(1 << 30); // e.g., 1GB
        let cfs = vec![
            latest_block_hash_cf,
            block_hash_to_block_info_cf,
            block_num_to_block_hash_cf,
            address_to_account_cf,
            address_to_storage_cf,
            hash_to_code_cf,
        ];
        let db = DB::open_cf_descriptors(&db_opts, path, cfs).unwrap();
        Self { db }
    }
}
