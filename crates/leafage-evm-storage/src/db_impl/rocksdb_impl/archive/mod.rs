//! RocksDB database implementation for the archive storage.
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
//! The `BlockHashToBlockInfo` column family stores the block hash to block header maps.
//! The `BlockNumToBlockHash` column family stores the block number to block hash maps.
//! The `AddressToAccount` column family stores the (address,BlockNumber) to account maps.
//! The `AddressToStorage` column family stores the (address,index,BlockNumber) to storage maps.
//! The `HashToCode` column family stores the code hash to code maps.
//! All [`U256`] are big-endian encoded.

//! ArchiveRocksDB is a RocksDB database implementation for the archive storage.
//! ArchiveRocksDB not support the following operations:
//! - `get_transaction_by_hash`
//! - `get_transaction_by_context`

use crate::db::{
    ArchiveDBProvider, BlockIterator, BlockRead, StateDBIterator, StateDBRead, StateDBWrite,
};
use crate::metrics::STORAGE_METRICS;
use alloy::primitives::B64;
use alloy::rpc::types::ConversionError;
use alloy_rlp::{Decodable, Encodable};
use leafage_evm_types::{
    Block, BlockId, BlockNumberOrTag, Bytes, Header, NewAccount, RawHeader, SlimAccount, H256,
    KECCAK256_EMPTY, U256,
};
use revm::database_interface::DBErrorMarker;
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamily, ColumnFamilyDescriptor, DBRawIteratorWithThreadMode,
    IteratorMode, Options, ReadOptions, SliceTransform, WriteBatch, DB,
};
use std::fmt::Display;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::{env, u64};
use thiserror::Error;
use tracing::info;

static mut DATA_BASE: Option<DataBaseInner> = None;

#[derive(Debug, Error)]
pub enum Error {
    #[error("rocksdb error, {0}")]
    RocksDB(#[from] rocksdb::Error),
    #[error("rlp error, {0}")]
    Rlp(#[from] alloy_rlp::Error),
    #[error("unsupported operation, {0}")]
    UnSupported(String),
    #[error("unsupported block id, {0}")]
    UnsupportedBlockId(BlockId),
    #[error("conversion error, {0}")]
    Conversion(#[from] ConversionError),
}
impl DBErrorMarker for Error {}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StorageTypeColumn {
    // 1 -> block hash
    LatestBlockHash = 1,
    // block hash -> block header
    BlockHashToBlockInfo = 2,
    // block num -> block hash
    BlockNumToBlockHash = 3,
    // address || block num -> account
    AddressToAccount = 4,
    // address || storage index || block num -> storage
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

impl Display for StorageTypeColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_display())
    }
}

#[derive(Debug)]
struct DataBaseInner {
    _cols: Vec<(StorageTypeColumn, NonNull<ColumnFamily>)>,
    db: DB,
}

#[derive(Debug)]
pub struct DataBaseRef {
    db: &'static DB,
}

impl Drop for DataBaseRef {
    fn drop(&mut self) {
        unsafe {
            DATA_BASE.as_mut().unwrap().db.flush().unwrap();
            DATA_BASE = None;
        }
    }
}

unsafe impl Send for DataBaseInner {}
unsafe impl Sync for DataBaseInner {}

#[inline]
fn rocksdb_column_options(cache_size: usize, fixed_prefix_size: usize) -> Options {
    let mut cf_opts = Options::default();
    cf_opts.set_max_total_wal_size(1 << 28); // e.g., 256MB
    cf_opts.set_keep_log_file_num(2);
    cf_opts.set_level_compaction_dynamic_level_bytes(true);
    if fixed_prefix_size != 0 {
        cf_opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(fixed_prefix_size));
        cf_opts.set_memtable_prefix_bloom_ratio(0.1);
    }
    let mut block_opts = BlockBasedOptions::default();
    let cache = Cache::new_lru_cache(1024 * 1024 * cache_size);
    block_opts.set_block_cache(&cache);
    block_opts.set_cache_index_and_filter_blocks(true);
    block_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
    block_opts.set_pin_top_level_index_and_filter(true);
    block_opts.set_bloom_filter(10.0, fixed_prefix_size != 0);
    block_opts.set_index_type(rocksdb::BlockBasedIndexType::TwoLevelIndexSearch);
    block_opts.set_partition_filters(true);
    block_opts.set_metadata_block_size(4096);
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
    opts.increase_parallelism(2);

    if let Ok(max_open_file_string) = env::var("ROCKSDB_MAX_OPEN_FILE") {
        if let Ok(max_open_file) = max_open_file_string.parse::<i32>() {
            opts.set_max_open_files(max_open_file);
            info!(
                target = "rocksdb",
                "set rocksdb max open file to {}", max_open_file
            );
        }
    }

    if let Ok(set_direct_io) = env::var("ROCKSDB_DIRECT_IO") {
        if set_direct_io == "1" || set_direct_io == "true" || set_direct_io == "TRUE" {
            opts.set_use_direct_reads(true);
            info!(target = "rocksdb", "set rocksdb use direct reads");
        }
    }
    // Disabling dumping stats to files because the stats are exported to
    // Prometheus.
    opts.set_stats_persist_period_sec(0);
    opts.set_stats_dump_period_sec(0);

    opts
}

impl DataBaseRef {
    pub fn open<P: AsRef<Path>>(path: P, cache_size: usize) -> Self {
        let latest_block_hash_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::LatestBlockHash.to_str(),
            rocksdb_column_options(32, 0),
        );
        let block_hash_to_block_info_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::BlockHashToBlockInfo.to_str(),
            rocksdb_column_options(64, 0),
        );
        let block_num_to_block_hash_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::BlockNumToBlockHash.to_str(),
            rocksdb_column_options(64, 0),
        );
        let address_to_account_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToAccount.to_str(),
            rocksdb_column_options(cache_size / 5, 32),
        );
        let address_to_storage_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToStorage.to_str(),
            rocksdb_column_options(cache_size, 64),
        );
        let hash_to_code_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::HashToCode.to_str(),
            rocksdb_column_options(cache_size / 5, 0),
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
        let storage_ref = db
            .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
            .unwrap();
        let ssts = db.get_column_family_sst_name_by_levels(storage_ref);
        info!(
            target = "rocksdb",
            "open rocksdb, address_to_storage sst files: {:?}", ssts
        );
        unsafe { DATA_BASE = Some(DataBaseInner { _cols: cols, db }) }
        Self {
            db: unsafe { &DATA_BASE.as_ref().unwrap().db },
        }
    }
}

impl BlockRead for DataBaseRef {
    type Error = Error;

    fn read_block_hash(&self, block_num: u64) -> Result<H256, Error> {
        let start = std::time::Instant::now();
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
        STORAGE_METRICS
            .read_block_hash_latency
            .record(start.elapsed().as_secs_f64());
        if block_hash_bytes.is_none() {
            return Ok(H256::ZERO);
        }
        let block_hash_bytes = block_hash_bytes.unwrap();
        let block_hash = H256::from_slice(block_hash_bytes.as_ref());
        Ok(block_hash)
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<Block<H256>>, Error> {
        let start = std::time::Instant::now();
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
        STORAGE_METRICS
            .read_block_latency
            .record(start.elapsed().as_secs_f64());
        if block_info_bytes.is_none() {
            return Ok(None);
        }
        let block_info_bytes = block_info_bytes.unwrap();
        let mut block_header = RawHeader::decode(&mut block_info_bytes.as_ref());
        if block_header.is_err() {
            let buf = &mut block_info_bytes.as_ref();
            let rlp_head = alloy_rlp::Header::decode(buf)?;
            if !rlp_head.list {
                Err(alloy_rlp::Error::NonCanonicalSingleByte)?;
            }
            let header = RawHeader {
                parent_hash: Decodable::decode(buf)?,
                ommers_hash: Decodable::decode(buf)?,
                beneficiary: Decodable::decode(buf)?,
                state_root: Decodable::decode(buf)?,
                transactions_root: Decodable::decode(buf)?,
                receipts_root: Decodable::decode(buf)?,
                logs_bloom: Decodable::decode(buf)?,
                difficulty: Decodable::decode(buf)?,
                number: u64::decode(buf)?,
                gas_limit: u64::decode(buf)?,
                gas_used: u64::decode(buf)?,
                timestamp: Decodable::decode(buf)?,
                extra_data: Decodable::decode(buf)?,
                mix_hash: Decodable::decode(buf)?,
                nonce: B64::decode(buf)?,
                ..Default::default()
            };
            block_header = Ok(header);
        }
        let block_header = block_header.unwrap();
        let block = Block {
            header: Header {
                hash: block_hash,
                inner: block_header,
                ..Default::default()
            },
            ..Default::default()
        };
        Ok(Some(block))
    }

    fn read_latest_block_hash(&self) -> Result<H256, Error> {
        let start = std::time::Instant::now();
        let latest_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
            .unwrap();
        let block_hash_bytes = self.db.get_cf_opt(
            latest_block_hash_cf,
            [1u8].to_vec(),
            &rocksdb_read_options(),
        )?;
        STORAGE_METRICS
            .read_latest_block_hash_latency
            .record(start.elapsed().as_secs_f64());
        if block_hash_bytes.is_none() {
            return Ok(H256::ZERO);
        }
        let block_hash_bytes = block_hash_bytes.unwrap();
        let block_hash = H256::from_slice(block_hash_bytes.as_slice());
        Ok(block_hash)
    }
}

impl StateDBIterator for DataBaseRef {
    type Error = Error;
    /// account address -> raw account
    fn account_iter(&self) -> impl Iterator<Item = Result<(H256, NewAccount), Self::Error>> {
        self.db
            .iterator_cf_opt(
                self.db
                    .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
                    .unwrap(),
                rocksdb_read_options(),
                IteratorMode::Start,
            )
            .filter_map(|item| {
                if item.is_err() {
                    return Some(Err(Error::RocksDB(item.unwrap_err())));
                }
                let (key, value) = item.unwrap();
                let block_num_bytes = &key[32..];
                let block_num = U256::from_be_slice(block_num_bytes);
                if block_num != U256::from(u64::MAX) {
                    return None;
                }
                let address_bytes = &key[..32];
                let address = H256::from_slice(address_bytes);
                let mut raw_account_slice = value.as_ref();
                let raw_account = SlimAccount::decode(&mut raw_account_slice).unwrap();
                let account = NewAccount {
                    address,
                    balance: raw_account.balance,
                    nonce: raw_account.nonce,
                    code_hash: if raw_account.code_hash.is_zero() {
                        KECCAK256_EMPTY.0.into()
                    } else {
                        raw_account.code_hash
                    },
                };
                Some(Ok((address, account)))
            })
    }

    /// code hash -> code
    fn code_iter(&self) -> impl Iterator<Item = Result<(H256, Bytes), Self::Error>> {
        self.db
            .iterator_cf_opt(
                self.db
                    .cf_handle(StorageTypeColumn::HashToCode.to_str())
                    .unwrap(),
                rocksdb_read_options(),
                IteratorMode::Start,
            )
            .map(|item| {
                if item.is_err() {
                    return Err(Error::RocksDB(item.unwrap_err()));
                }
                let (key, value) = item.unwrap();
                let code_hash = H256::from_slice(key.as_ref());
                Ok((code_hash, Bytes::from(value)))
            })
    }

    /// account address | storage index -> storage value
    fn storage_iter(&self) -> impl Iterator<Item = Result<(H256, H256, U256), Self::Error>> {
        self.db
            .iterator_cf_opt(
                self.db
                    .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
                    .unwrap(),
                rocksdb_read_options(),
                IteratorMode::Start,
            )
            .filter_map(|item| {
                if item.is_err() {
                    return Some(Err(Error::RocksDB(item.unwrap_err())));
                }
                let (key, value) = item.unwrap();
                let block_num_bytes = &key[64..];
                let block_num = U256::from_be_slice(block_num_bytes);
                if block_num != U256::from(u64::MAX) {
                    return None;
                }
                let address_bytes = &key[..32];
                let address = H256::from_slice(address_bytes);
                let key_bytes = &key[32..64];
                let key = H256::from_slice(key_bytes);
                let value = U256::from_be_slice(value.as_ref());
                Some(Ok((address, key, value)))
            })
    }
}

impl BlockIterator for DataBaseRef {
    type Error = Error;
    fn block_info_iter(&self) -> impl Iterator<Item = Result<Block<H256>, Self::Error>> {
        self.db
            .iterator_cf_opt(
                self.db
                    .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
                    .unwrap(),
                rocksdb_read_options(),
                IteratorMode::Start,
            )
            .map(|item| {
                if item.is_err() {
                    return Err(Error::RocksDB(item.unwrap_err()));
                }
                let (key, value) = item.unwrap();
                let block_hash = H256::from_slice(key.as_ref());
                let block_header: RawHeader = RawHeader::decode(&mut value.as_ref()).unwrap();
                let block = Block {
                    header: Header {
                        hash: block_hash,
                        inner: block_header,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                Ok(block)
            })
    }

    fn block_hash_iter(&self) -> impl Iterator<Item = Result<(u64, H256), Self::Error>> {
        self.db
            .iterator_cf_opt(
                self.db
                    .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
                    .unwrap(),
                rocksdb_read_options(),
                IteratorMode::Start,
            )
            .map(|item| {
                if item.is_err() {
                    return Err(Error::RocksDB(item.unwrap_err()));
                }
                let (key, value) = item.unwrap();
                let block_num: u64 = U256::from_be_slice(key.as_ref()).try_into().unwrap();
                let block_hash = H256::from_slice(value.as_ref());
                Ok((block_num, block_hash))
            })
    }
}

impl ArchiveDBProvider for Arc<DataBaseRef> {
    type StateDBReadWrite = StateDB;
    fn db_at(&self, block_id: BlockId) -> Result<Option<Self::StateDBReadWrite>, Error> {
        let block_num: u64;
        let block_header: Header;
        match block_id {
            BlockId::Hash(hash) => {
                let header = self.read_block_info(hash.block_hash)?;
                if header.is_none() {
                    return Ok(None);
                }
                block_header = header.unwrap().header;
                block_num = block_header.number;
            }
            BlockId::Number(block_number_or_tag) => match block_number_or_tag {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                    block_num = u64::MAX;
                    return Ok(Some(StateDB {
                        db: self.clone(),
                        block_num,
                        block_header: None,
                        account_iterator: None,
                        storage_iterator: None,
                    }));
                }
                BlockNumberOrTag::Number(num) => {
                    block_num = num;
                    let block_hash = self.read_block_hash(num)?;
                    if block_hash == H256::ZERO {
                        return Ok(None);
                    }
                    let header = self.read_block_info(block_hash)?;
                    if header.is_none() {
                        return Ok(None);
                    }
                    block_header = header.unwrap().header;
                }
                _ => return Err(Error::UnsupportedBlockId(block_id)),
            },
        }

        let address_to_account_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
            .unwrap();
        let account_iterator = self
            .db
            .raw_iterator_cf_opt(address_to_account_cf, rocksdb_read_options());

        let address_to_storage_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
            .unwrap();

        let storage_iterator = self
            .db
            .raw_iterator_cf_opt(address_to_storage_cf, rocksdb_read_options());
        Ok(Some(StateDB {
            db: self.clone(),
            block_num,
            block_header: Some(block_header),
            account_iterator: Some(Mutex::new(account_iterator)),
            storage_iterator: Some(Mutex::new(storage_iterator)),
        }))
    }
}

pub struct StateDB {
    db: Arc<DataBaseRef>,
    block_num: u64,
    block_header: Option<Header>,
    account_iterator: Option<Mutex<DBRawIteratorWithThreadMode<'static, DB>>>,
    storage_iterator: Option<Mutex<DBRawIteratorWithThreadMode<'static, DB>>>,
}

impl Clone for StateDB {
    fn clone(&self) -> Self {
        if self.block_num == u64::MAX {
            return Self {
                db: self.db.clone(),
                block_num: self.block_num,
                block_header: self.block_header.clone(),
                account_iterator: None,
                storage_iterator: None,
            };
        }

        let address_to_account_cf = self
            .db
            .db
            .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
            .unwrap();
        let account_iterator = self
            .db
            .db
            .raw_iterator_cf_opt(address_to_account_cf, rocksdb_read_options());

        let address_to_storage_cf = self
            .db
            .db
            .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
            .unwrap();

        let storage_iterator = self
            .db
            .db
            .raw_iterator_cf_opt(address_to_storage_cf, rocksdb_read_options());
        Self {
            db: self.db.clone(),
            block_num: self.block_num,
            block_header: self.block_header.clone(),
            account_iterator: Some(Mutex::new(account_iterator)),
            storage_iterator: Some(Mutex::new(storage_iterator)),
        }
    }
}

unsafe impl Send for StateDB {}
unsafe impl Sync for StateDB {}

impl StateDBRead for StateDB {
    type Error = Error;

    fn read_account(&self, address: H256) -> Result<Option<NewAccount>, Error> {
        let start = std::time::Instant::now();
        let address_bytes: [u8; 32] = address.into();
        let block_num_bytes: [u8; 32] = U256::from(self.block_num).to_be_bytes();
        if self.block_num == u64::MAX {
            let address_to_account_cf = self
                .db
                .db
                .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
                .unwrap();
            let raw_account_bytes = self.db.db.get_pinned_cf_opt(
                address_to_account_cf,
                [address_bytes.as_ref(), &block_num_bytes].concat(),
                &rocksdb_read_options(),
            )?;
            STORAGE_METRICS
                .read_account_latency
                .record(start.elapsed().as_secs_f64());
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
                    KECCAK256_EMPTY.0.into()
                } else {
                    account.code_hash
                },
            };
            return Ok(Some(account));
        }
        let mut account_iter = self.account_iterator.as_ref().unwrap().lock().unwrap();
        account_iter.seek_for_prev([address_bytes.as_ref(), &block_num_bytes].concat());
        STORAGE_METRICS
            .read_account_latency
            .record(start.elapsed().as_secs_f64());
        if let Some(raw_key_bytes) = account_iter.key() {
            if address_bytes != raw_key_bytes[..32] {
                return Ok(None);
            }
            let mut raw_val_bytes = account_iter.value().unwrap();
            if raw_val_bytes.is_empty() {
                return Ok(None);
            }
            let account = SlimAccount::decode(&mut raw_val_bytes).unwrap();
            let account = NewAccount {
                address,
                balance: account.balance,
                nonce: account.nonce,
                code_hash: if account.code_hash.is_zero() {
                    KECCAK256_EMPTY.0.into()
                } else {
                    account.code_hash
                },
            };
            Ok(Some(account))
        } else {
            Ok(None)
        }
    }

    fn read_storage(&self, address: H256, key: H256) -> Result<U256, Error> {
        let start = std::time::Instant::now();
        let address_bytes: [u8; 32] = address.into();
        let key_bytes: [u8; 32] = key.into();
        let block_num_bytes: [u8; 32] = U256::from(self.block_num).to_be_bytes();
        if self.block_num == u64::MAX {
            let address_to_storage_cf = self
                .db
                .db
                .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
                .unwrap();
            let value_bytes = self.db.db.get_pinned_cf_opt(
                address_to_storage_cf,
                [address_bytes.as_ref(), &key_bytes, &block_num_bytes].concat(),
                &rocksdb_read_options(),
            )?;
            STORAGE_METRICS
                .read_storage_latency
                .record(start.elapsed().as_secs_f64());
            if value_bytes.is_none() {
                return Ok(U256::ZERO);
            }
            let value_bytes = value_bytes.unwrap();
            let value = U256::from_be_slice(value_bytes.as_ref());
            return Ok(value);
        }
        let mut storage_iter = self.storage_iterator.as_ref().unwrap().lock().unwrap();
        storage_iter
            .seek_for_prev([address_bytes.as_ref(), key_bytes.as_ref(), &block_num_bytes].concat());
        STORAGE_METRICS
            .read_storage_latency
            .record(start.elapsed().as_secs_f64());
        if let Some(raw_key_bytes) = storage_iter.key() {
            if address_bytes != raw_key_bytes[..32] {
                return Ok(U256::ZERO);
            }
            if key_bytes != raw_key_bytes[32..64] {
                return Ok(U256::ZERO);
            }
            let raw_val_bytes = storage_iter.value().unwrap();
            if raw_val_bytes.is_empty() {
                return Ok(U256::ZERO);
            }
            let value = U256::from_be_slice(&raw_val_bytes);
            Ok(value)
        } else {
            Ok(U256::ZERO)
        }
    }

    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, Error> {
        let start = std::time::Instant::now();
        let address_to_code_cf = self
            .db
            .db
            .cf_handle(StorageTypeColumn::HashToCode.to_str())
            .unwrap();
        let code_hash_bytes: [u8; 32] = code_hash.into();
        let code =
            self.db
                .db
                .get_cf_opt(address_to_code_cf, code_hash_bytes, &rocksdb_read_options())?;
        STORAGE_METRICS
            .read_code_latency
            .record(start.elapsed().as_secs_f64());
        if code.is_none() {
            return Ok(None);
        }
        Ok(Some(Bytes::from(code.unwrap())))
    }
}

impl BlockRead for StateDB {
    type Error = Error;

    fn read_block_hash(&self, block_num: u64) -> Result<H256, Error> {
        if block_num == self.block_num && self.block_header.is_some() {
            return Ok(self.block_header.as_ref().unwrap().hash);
        }
        self.db.read_block_hash(block_num)
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<Block<H256>>, Error> {
        if self.block_header.is_some() && block_hash == self.block_header.as_ref().unwrap().hash {
            return Ok(Some(Block {
                header: self.block_header.as_ref().unwrap().clone(),
                ..Default::default()
            }));
        }
        self.db.read_block_info(block_hash)
    }
    fn read_latest_block_hash(&self) -> Result<H256, Error> {
        if self.block_num == u64::MAX {
            return Ok(self.db.read_latest_block_hash()?);
        }
        Ok(self.block_header.as_ref().unwrap().hash)
    }
}
impl StateDBWrite for StateDB {
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
        block_info: Block<H256>,
    ) -> Result<(), Error> {
        let block_hash_to_block_info_cf = self
            .db
            .db
            .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
            .unwrap();
        let block_hash_bytes: [u8; 32] = block_info.header.hash.into();
        let mut block_info_bytes = Vec::new();
        let block_header: RawHeader = block_info.header.inner;
        block_header.encode(&mut block_info_bytes);
        batch.put_cf(
            block_hash_to_block_info_cf,
            block_hash_bytes,
            block_info_bytes,
        );
        Ok(())
    }

    fn write_account(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        block_num: u64,
        raw_account: Option<NewAccount>,
    ) -> Result<(), Error> {
        let address_to_account_cf = self
            .db
            .db
            .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
            .unwrap();
        let address_bytes = address.as_slice();
        let block_num_bytes: [u8; 32] = U256::from(block_num).to_be_bytes();
        let max_block_num_bytes: [u8; 32] = U256::from(u64::MAX).to_be_bytes();
        if let Some(raw_account) = raw_account {
            let raw_account: SlimAccount = raw_account.into();
            let mut raw_account_bytes = Vec::new();
            raw_account.encode(&mut raw_account_bytes);
            batch.put_cf(
                address_to_account_cf,
                [address_bytes, &block_num_bytes].concat(),
                &raw_account_bytes,
            );
            batch.put_cf(
                address_to_account_cf,
                [address_bytes, &max_block_num_bytes].concat(),
                &raw_account_bytes,
            );
        } else {
            batch.put_cf(
                address_to_account_cf,
                [address_bytes, &block_num_bytes].concat(),
                &[],
            );
            batch.delete_cf(
                address_to_account_cf,
                [address_bytes, &max_block_num_bytes].concat(),
            );
        }
        Ok(())
    }

    fn write_storage(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        key: H256,
        block_num: u64,
        value: U256,
    ) -> Result<(), Error> {
        let address_to_storage_cf = self
            .db
            .db
            .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
            .unwrap();
        let address_bytes = address.as_slice();
        let key_bytes: [u8; 32] = key.into();
        let block_num_bytes: [u8; 32] = U256::from(block_num).to_be_bytes();
        let max_block_num_bytes: [u8; 32] = U256::from(u64::MAX).to_be_bytes();
        let value_bytes: [u8; 32] = value.to_be_bytes();
        if value == U256::ZERO {
            batch.delete_cf(
                address_to_storage_cf,
                [address_bytes, &key_bytes, &max_block_num_bytes].concat(),
            );
        } else {
            batch.put_cf(
                address_to_storage_cf,
                [address_bytes, &key_bytes, &max_block_num_bytes].concat(),
                value_bytes,
            );
        }
        batch.put_cf(
            address_to_storage_cf,
            [address_bytes, &key_bytes, &block_num_bytes].concat(),
            value_bytes,
        );
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
            .db
            .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
            .unwrap();
        batch.put_cf(latest_block_hash_cf, [1u8].to_vec(), block_hash.as_slice());
        Ok(())
    }

    fn commit(&self, batch: Self::DBWriteBatch) -> Result<(), Self::Error> {
        self.db.db.write(batch)?;
        Ok(())
    }
}
