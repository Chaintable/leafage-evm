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
use crate::metrics::{DATABASE_CACHE_USAGE, DATABASE_OP_LATENCY_HIST};
use alloy_rlp::{Decodable, Encodable};
use leafage_evm_types::{
    Block, Bytes, NewAccount, SlimAccount, Transaction, H256, KECCAK_EMPTY, U256,
};
use rocksdb::{
    properties, BlockBasedOptions, Cache, ColumnFamily, ColumnFamilyDescriptor, Options,
    ReadOptions, WriteBatch, DB,
};
use serde_json::{from_slice, to_vec};
use std::path::Path;
use std::ptr::NonNull;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("rocksdb error, {0}")]
    RocksDB(#[from] rocksdb::Error),
    #[error("serde_json error, {0}")]
    SerdeJson(#[from] serde_json::Error),
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
#[inline]
fn rocksdb_read_options() -> ReadOptions {
    let mut read_options = ReadOptions::default();
    read_options.set_verify_checksums(false);
    read_options
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

    fn to_display(&self) -> &'static str {
        match self {
            StorageTypeColumn::LatestBlockHash => "LatestBlockHash",
            StorageTypeColumn::BlockHashToBlockInfo => "BlockHashToBlockInfo",
            StorageTypeColumn::BlockNumToBlockHash => "BlockNumToBlockHash",
            StorageTypeColumn::AddressToAccount => "AddressToAccount",
            StorageTypeColumn::AddressToStorage => "AddressToStorage",
            StorageTypeColumn::HashToCode => "HashToCode",
        }
    }
}

pub struct DataBase {
    db: DB,
    cols: Vec<(StorageTypeColumn, NonNull<ColumnFamily>)>,
}

unsafe impl Send for DataBase {}
unsafe impl Sync for DataBase {}

impl StateDBRead for DataBase {
    type Error = Error;
    fn read_block_hash(&self, block_num: u64) -> Result<H256, Error> {
        let timer = DATABASE_OP_LATENCY_HIST
            .with_label_values(&["read", StorageTypeColumn::BlockNumToBlockHash.to_display()])
            .start_timer();
        let block_num_to_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .unwrap();
        let block_num_bytes: [u8; 32] = U256::from(block_num).to_be_bytes();
        let block_hash_bytes = self.db.get_pinned_cf_opt(
            block_num_to_block_hash_cf,
            block_num_bytes,
            &rocksdb_read_options(),
        )?;
        timer.observe_duration();
        if block_hash_bytes.is_none() {
            return Ok(H256::ZERO);
        }
        let block_hash_bytes = block_hash_bytes.unwrap();
        let block_hash = H256::from_slice(block_hash_bytes.as_ref());
        Ok(block_hash)
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<Block<Transaction>>, Error> {
        let timer = DATABASE_OP_LATENCY_HIST
            .with_label_values(&["read", StorageTypeColumn::BlockHashToBlockInfo.to_display()])
            .start_timer();
        let block_hash_to_block_info_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
            .unwrap();
        let block_hash_bytes: [u8; 32] = block_hash.into();
        let block_info_bytes = self.db.get_pinned_cf_opt(
            block_hash_to_block_info_cf,
            block_hash_bytes,
            &rocksdb_read_options(),
        )?;
        timer.observe_duration();
        if block_info_bytes.is_none() {
            return Ok(None);
        }
        let block_info_bytes = block_info_bytes.unwrap();
        let block_info_slice = block_info_bytes.as_ref();
        let block_info = from_slice::<Block<Transaction>>(block_info_slice)?;
        Ok(Some(block_info))
    }

    fn read_account(&self, address: H256) -> Result<Option<NewAccount>, Error> {
        let timer = DATABASE_OP_LATENCY_HIST
            .with_label_values(&["read", StorageTypeColumn::AddressToAccount.to_display()])
            .start_timer();
        let address_to_account_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
            .unwrap();
        let address_bytes: [u8; 32] = address.into();
        let raw_account_bytes = self.db.get_pinned_cf_opt(
            address_to_account_cf,
            address_bytes,
            &rocksdb_read_options(),
        )?;
        timer.observe_duration();
        if raw_account_bytes.is_none() {
            return Ok(None);
        }
        let raw_account_bytes = raw_account_bytes.unwrap();
        let mut raw_account_slice = raw_account_bytes.as_ref();
        let account = SlimAccount::decode(&mut raw_account_slice).unwrap();
        let account = NewAccount {
            address,
            balance: account.balance,
            nonce: account.nonce,
            code_hash: if account.code_hash.is_zero() {
                KECCAK_EMPTY.0.into()
            } else {
                account.code_hash
            },
        };
        Ok(Some(account))
    }

    fn read_storage(&self, address: H256, key: H256) -> Result<U256, Error> {
        let timer = DATABASE_OP_LATENCY_HIST
            .with_label_values(&["read", StorageTypeColumn::AddressToStorage.to_display()])
            .start_timer();
        let address_to_storage_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
            .unwrap();
        let address_bytes: [u8; 32] = address.into();
        let key_bytes: [u8; 32] = key.into();
        let value_bytes = self.db.get_pinned_cf_opt(
            address_to_storage_cf,
            [address_bytes.as_ref(), &key_bytes].concat(),
            &rocksdb_read_options(),
        )?;
        timer.observe_duration();
        if value_bytes.is_none() {
            return Ok(U256::ZERO);
        }
        let value_bytes = value_bytes.unwrap();
        let value = U256::from_be_slice(value_bytes.as_ref());
        Ok(value)
    }

    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, Error> {
        let timer = DATABASE_OP_LATENCY_HIST
            .with_label_values(&["read", StorageTypeColumn::HashToCode.to_display()])
            .start_timer();
        let address_to_code_cf = self
            .db
            .cf_handle(StorageTypeColumn::HashToCode.to_str())
            .unwrap();
        let code_hash_bytes: [u8; 32] = code_hash.into();
        let code =
            self.db
                .get_cf_opt(address_to_code_cf, code_hash_bytes, &rocksdb_read_options())?;
        timer.observe_duration();
        if code.is_none() {
            return Ok(None);
        }
        Ok(Some(Bytes::from(code.unwrap())))
    }

    fn read_latest_block_hash(&self) -> Result<H256, Error> {
        let timer = DATABASE_OP_LATENCY_HIST
            .with_label_values(&["read", StorageTypeColumn::LatestBlockHash.to_display()])
            .start_timer();
        let latest_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
            .unwrap();
        let block_hash_bytes = self.db.get_cf_opt(
            latest_block_hash_cf,
            [1u8].to_vec(),
            &rocksdb_read_options(),
        )?;
        timer.observe_duration();
        if block_hash_bytes.is_none() {
            return Ok(H256::ZERO);
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
        block_num: u64,
        block_hash: H256,
    ) -> Result<(), Self::Error> {
        let block_num_to_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .unwrap();
        let block_hash_bytes: [u8; 32] = block_hash.into();
        let block_num_bytes: [u8; 32] = U256::from(block_num).to_be_bytes();
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
        batch.delete_cf(
            block_hash_to_block_info_cf,
            block_info.header.parent_hash.as_slice(),
        );
        let block_info_bytes = to_vec(&block_info)?;
        let block_hash = block_info.header.hash;
        batch.put_cf(
            block_hash_to_block_info_cf,
            block_hash.as_slice(),
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
        let address_bytes = address.as_slice();
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
        let address_bytes = address.as_slice();
        let key_bytes: [u8; 32] = key.into();
        if value == U256::ZERO {
            batch.delete_cf(address_to_storage_cf, [address_bytes, &key_bytes].concat());
            return Ok(());
        } else {
            let value_bytes: [u8; 32] = value.to_be_bytes();
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
        let code_hash_bytes = code_hash.as_slice();
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
        batch.put_cf(latest_block_hash_cf, [1u8].to_vec(), block_hash.as_slice());
        Ok(())
    }

    fn commit(&self, batch: Self::DBWriteBatch) -> Result<(), Self::Error> {
        let timer = DATABASE_OP_LATENCY_HIST
            .with_label_values(&["write", "all"])
            .start_timer();
        self.db.write(batch)?;
        timer.observe_duration();
        Ok(())
    }
}

#[inline]
fn rocksdb_column_options(cache_size: usize) -> Options {
    let mut cf_opts = Options::default();
    cf_opts.set_max_total_wal_size(1 << 28); // e.g., 256MB
    cf_opts.set_keep_log_file_num(2);
    cf_opts.set_level_compaction_dynamic_level_bytes(true);
    let mut block_opts = BlockBasedOptions::default();
    let cache = Cache::new_lru_cache(1024 * 1024 * cache_size);
    block_opts.set_block_cache(&cache);
    block_opts.set_cache_index_and_filter_blocks(true);
    block_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
    block_opts.set_pin_top_level_index_and_filter(true);
    block_opts.set_bloom_filter(10.0, false);
    cf_opts.set_block_based_table_factory(&block_opts);
    cf_opts.optimize_level_style_compaction(1 << 28); // e.g., 256MB
    cf_opts
}

#[inline]
fn rocksdb_options() -> Options {
    let mut opts = Options::default();
    opts.create_missing_column_families(true);
    opts.create_if_missing(true);
    opts.set_use_fsync(false);
    opts.set_keep_log_file_num(1);
    opts.set_bytes_per_sync(1 << 20); // e.g., 1MB
    opts.set_write_buffer_size(1 << 28); // e.g., 256MB
    opts.set_max_bytes_for_level_base(1 << 28); // e.g., 256MB
    opts.set_max_total_wal_size(1 << 29); // e.g., 512MB
    opts.enable_statistics();
    opts.increase_parallelism(2);
    // Disabling dumping stats to files because the stats are exported to
    // Prometheus.
    opts.set_stats_persist_period_sec(0);
    opts.set_stats_dump_period_sec(0);

    opts
}

impl DataBase {
    pub fn open<P: AsRef<Path>>(path: P, cache_size: usize) -> Self {
        let latest_block_hash_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::LatestBlockHash.to_str(),
            rocksdb_column_options(32),
        );
        let block_hash_to_block_info_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::BlockHashToBlockInfo.to_str(),
            rocksdb_column_options(64),
        );
        let block_num_to_block_hash_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::BlockNumToBlockHash.to_str(),
            rocksdb_column_options(64),
        );
        let address_to_account_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToAccount.to_str(),
            rocksdb_column_options(cache_size / 5),
        );
        let address_to_storage_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToStorage.to_str(),
            rocksdb_column_options(cache_size),
        );
        let hash_to_code_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::HashToCode.to_str(),
            rocksdb_column_options(cache_size / 5),
        );
        let cfs = vec![
            latest_block_hash_cf,
            block_hash_to_block_info_cf,
            block_num_to_block_hash_cf,
            address_to_account_cf,
            address_to_storage_cf,
            hash_to_code_cf,
        ];
        let db_opt = rocksdb_options();
        let db = DB::open_cf_descriptors(&db_opt, path, cfs).unwrap();
        let cols = vec![
            (
                StorageTypeColumn::LatestBlockHash,
                NonNull::new(
                    db.cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
                        .unwrap() as *const _ as *mut _,
                )
                .unwrap(),
            ),
            (
                StorageTypeColumn::BlockHashToBlockInfo,
                NonNull::new(
                    db.cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
                        .unwrap() as *const _ as *mut _,
                )
                .unwrap(),
            ),
            (
                StorageTypeColumn::BlockNumToBlockHash,
                NonNull::new(
                    db.cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
                        .unwrap() as *const _ as *mut _,
                )
                .unwrap(),
            ),
            (
                StorageTypeColumn::AddressToAccount,
                NonNull::new(
                    db.cf_handle(StorageTypeColumn::AddressToAccount.to_str())
                        .unwrap() as *const _ as *mut _,
                )
                .unwrap(),
            ),
            (
                StorageTypeColumn::AddressToStorage,
                NonNull::new(
                    db.cf_handle(StorageTypeColumn::AddressToStorage.to_str())
                        .unwrap() as *const _ as *mut _,
                )
                .unwrap(),
            ),
            (
                StorageTypeColumn::HashToCode,
                NonNull::new(
                    db.cf_handle(StorageTypeColumn::HashToCode.to_str())
                        .unwrap() as *const _ as *mut _,
                )
                .unwrap(),
            ),
        ];
        Self { db, cols }
    }

    pub fn report_cache_usage(&self) {
        for (col, column_family) in self.cols.iter() {
            let handle = unsafe { column_family.as_ref() };
            let prop = self
                .db
                .property_int_value_cf(handle, properties::BLOCK_CACHE_USAGE);
            if let Ok(Some(prop)) = prop {
                DATABASE_CACHE_USAGE
                    .with_label_values(&[col.to_display()])
                    .set(prop as i64);
            }
        }
    }
}
