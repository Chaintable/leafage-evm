//! RocksDB implementation of the database.
//!
//! Data is stored in the following format:
//! ```text
//! +-------------------------+--------------------------------------+-------------------------+
//! |  LatestBlockHash        |  BlockHashToBlockInfo (only latest)  |  BlockNumToBlockHash    |
//! +-------------------------+--------------------------------------+-------------------------+
//! |  AddressToAccount       |  AddressToStorage                    |  HashToCode             |
//! +-------------------------+--------------------------------------+-------------------------+
//! ```
//! The `LatestBlockHash` column family stores the latest block hash.
//! The `BlockHashToBlockInfo` column family stores the block hash to block info maps.
//! The `BlockNumToBlockHash` column family stores the block number to block hash maps.
//! The `AddressToAccount` column family stores the address to account maps.
//! The `AddressToStorage` column family stores the (address,index) to storage maps.
//! The `HashToCode` column family stores the code hash to code maps.
//! All [`U256`] are big-endian encoded.

use crate::db::{BlockRead, StateDBRead, StateDBWrite};
use crate::metrics::STORAGE_METRICS;
use alloy_rlp::{Decodable, Encodable};
use leafage_evm_types::{Block, Bytes, NewAccount, SlimAccount, H256, KECCAK256_EMPTY, U256};
use revm::database_interface::DBErrorMarker;
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamily, ColumnFamilyDescriptor, IteratorMode, Options, ReadOptions,
    WriteBatch, DB,
};
use serde_json::{from_slice, to_vec};
use std::env;
use std::fmt::Display;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tracing::info;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use memmap2::MmapOptions;

#[derive(Debug, Error)]
pub enum Error {
    #[error("rocksdb error, {0}")]
    RocksDB(#[from] rocksdb::Error),
    #[error("serde_json error, {0}")]
    SerdeJson(#[from] serde_json::Error),
}

impl DBErrorMarker for Error {}

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

impl Display for StorageTypeColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_display())
    }
}

pub struct DataBase {
    db: DB,
    _cols: Vec<(StorageTypeColumn, NonNull<ColumnFamily>)>,
}

unsafe impl Send for DataBase {}
unsafe impl Sync for DataBase {}

impl BlockRead for DataBase {
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
        let block_info_slice = block_info_bytes.as_ref();
        let block_info = from_slice::<Block<H256>>(block_info_slice)?;
        Ok(Some(block_info))
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

impl StateDBRead for DataBase {
    type Error = Error;

    fn read_account(&self, address: H256) -> Result<Option<NewAccount>, Error> {
        let start = std::time::Instant::now();
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
        Ok(Some(account))
    }

    fn read_storage(&self, address: H256, key: H256) -> Result<U256, Error> {
        let start = std::time::Instant::now();
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
        STORAGE_METRICS
            .read_storage_latency
            .record(start.elapsed().as_secs_f64());
        if value_bytes.is_none() {
            return Ok(U256::ZERO);
        }
        let value_bytes = value_bytes.unwrap();
        let value = U256::from_be_slice(value_bytes.as_ref());
        Ok(value)
    }

    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, Error> {
        let start = std::time::Instant::now();
        let address_to_code_cf = self
            .db
            .cf_handle(StorageTypeColumn::HashToCode.to_str())
            .unwrap();
        let code_hash_bytes: [u8; 32] = code_hash.into();
        let code =
            self.db
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
        block_info: Block<H256>,
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
        _block_num: u64,
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
        _block_num: u64,
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
        self.db.write(batch)?;
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

fn preheat_sst_file(file_path: &Path) -> io::Result<()> {
    let file = File::open(file_path)?;
    let mmap = unsafe { MmapOptions::new().map(&file)? };

    let mut sum = 0u8;
    for byte in mmap.iter().step_by(4096) {
        sum ^= *byte;
    }
    std::hint::black_box(sum);
    Ok(())
}

fn warmup_column_family_mmap(db: &DB, db_path: &Path, cf_name: &str, max_duration: Duration) {
    let start = std::time::Instant::now();

    if let Some(cf_handle) = db.cf_handle(cf_name) {
        let sst_by_levels = db.get_column_family_sst_name_by_levels(cf_handle);

        for (level, sst_files) in sst_by_levels {
            if start.elapsed() >= max_duration {
                break;
            }

            let total_files = sst_files.len();
            if total_files == 0 {
                println!("列族 {} Level {}: 无文件需要预热", cf_name, level);
                continue;
            }

            println!("列族 {} Level {}: {} 个文件待预热", cf_name, level, total_files);
            let mut preheated_count = 0;

            for sst_filename in sst_files {
                if start.elapsed() >= max_duration {
                    break;
                }

                let file_path = db_path.join(&sst_filename);
                if file_path.exists() {
                    if preheat_sst_file(&file_path).is_ok() {
                        preheated_count += 1;
                    }
                } else {
                    eprintln!("SST文件不存在: {:?}", file_path);
                }
            }

            println!("列族 {} Level {}: 预热完成，共 {} 个文件", cf_name, level, preheated_count);
        }
    } else {
        println!("警告: 列族 {} 句柄未找到", cf_name);
    }
}

fn warmup_column_family(db: &DB, cf_name: &str, max_duration: Duration) {
    let start = std::time::Instant::now();
    if let Some(cf_handle) = db.cf_handle(cf_name) {
        println!("开始预热列族 {} (最大时长 {:?})", cf_name, max_duration);

        // 简单地遍历整个列族，分两个阶段：元数据、数据
        let phases = [
            ("元数据(索引+过滤器)", WarmupPhase::Metadata),
            ("数据", WarmupPhase::Data),
        ];

        for (phase_name, phase) in phases.iter() {
            if start.elapsed() >= max_duration {
                println!("列族 {} 预热时间到达上限，停止在{}阶段", cf_name, phase_name);
                return;
            }

            let phase_start = std::time::Instant::now();
            println!("  阶段: 预热{}", phase_name);

            let keys_processed = warmup_phase_complete(db, cf_handle, *phase, &start, max_duration);

            println!("  阶段: {}预热完成，耗时 {:?}，遍历 {} 个键",
                     phase_name, phase_start.elapsed(), keys_processed);

            if start.elapsed() >= max_duration {
                println!("列族 {} 预热时间到达上限", cf_name);
                return;
            }
        }

        println!("列族 {} 所有阶段预热完成，总耗时 {:?}", cf_name, start.elapsed());
    } else {
        println!("警告: 列族 {} 句柄未找到", cf_name);
    }
}

fn warmup_phase_complete(
    db: &DB,
    cf_handle: &ColumnFamily,
    phase: WarmupPhase,
    global_start: &std::time::Instant,
    max_duration: Duration
) -> usize {
    let mut keys_processed = 0;
    let read_options = rocksdb_read_options();
    let iter = db.iterator_cf_opt(cf_handle, read_options, IteratorMode::Start);

    let check_interval = match phase {
        WarmupPhase::Metadata => 2000,
        WarmupPhase::Data => 5000,
    };

    for (i, item) in iter.enumerate() {
        if global_start.elapsed() >= max_duration {
            break;
        }

        if let Ok((key, value)) = item {
            match phase {
                WarmupPhase::Metadata => {
                    // 只读键，触发索引块和Bloom过滤器加载
                    std::hint::black_box(key);
                }
                WarmupPhase::Data => {
                    // 读键值对，触发数据块加载
                    std::hint::black_box((key, value));
                }
            }

            keys_processed += 1;

            // 定期检查超时
            if i % check_interval == 0 && i > 0 {
                if global_start.elapsed() >= max_duration {
                    break;
                }
            }
        }
    }

    keys_processed
}

#[derive(Debug, Copy, Clone)]
enum WarmupPhase {
    Metadata,
    Data,
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
        Self { db, _cols: cols }
    }

    pub fn spawn_warmup_with_mmap(db_arc: Arc<Self>, db_path: PathBuf, warmup_duration_secs: u64) {
        for cf_name in ["1", "2", "3", "4", "5", "6"] {
            let db_clone = db_arc.clone();
            let db_path_clone = db_path.clone();
            let cf = cf_name.to_string();
            std::thread::spawn(move || {
                warmup_column_family_mmap(&db_clone.db, &db_path_clone, &cf, Duration::from_secs(warmup_duration_secs));
            });
        }
    }

    pub fn spawn_warmup(db_arc: Arc<Self>, warmup_duration_secs: u64) {
        for cf_name in ["1", "2", "3", "4", "5", "6"] {
            let db_clone = db_arc.clone();
            let cf = cf_name.to_string();
            std::thread::spawn(move || {
                warmup_column_family(&db_clone.db, &cf, Duration::from_secs(warmup_duration_secs));
            });
        }
    }
}
