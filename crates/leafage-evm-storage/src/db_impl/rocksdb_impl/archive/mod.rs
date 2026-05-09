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

use crate::db::{BlockIterator, LatestStateDBIterator, StateDBProvider, StateDBRead, StateDBWrite};
use crate::db_impl::archive_encoding::{
    encode_account_key, encode_block_num, encode_slim_account, encode_storage_key,
};
use crate::db_impl::error::Error;
use crate::metrics::STORAGE_METRICS;
use alloy::primitives::B64;
use alloy_rlp::{Decodable, Encodable};
use leafage_evm_types::{
    Block, BlockId, BlockInfo, BlockNumberOrTag, Bytes, Header, NewAccount, RawHeader, SlimAccount,
    H256, KECCAK256_EMPTY, U256,
};
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamily, ColumnFamilyDescriptor, IteratorMode, Options,
    ReadOptions, SliceTransform, WriteBatch, WriteOptions, DB,
};
use std::env;
use std::fmt::{Debug, Display, Formatter};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;
use tracing::{info, warn};

mod iterator_tracker;
use iterator_tracker::{
    next_statedb_id, IteratorTracker, SharedIterators, TimeoutFlag, DEFAULT_ITERATOR_TIMEOUT_SECS,
};
use std::sync::atomic::Ordering;
use std::sync::LazyLock;

/// Global iterator tracker for StateDB instances
static ITERATOR_TRACKER: LazyLock<Arc<IteratorTracker>> = LazyLock::new(|| {
    let timeout_secs = std::env::var("ROCKSDB_ITERATOR_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_ITERATOR_TIMEOUT_SECS);

    let tracker = IteratorTracker::new(timeout_secs);

    if tracker.is_enabled() {
        info!(
            target: "rocksdb",
            timeout_secs = timeout_secs,
            "Initialized iterator tracker"
        );
        tracker.clone().start_monitor();
    }

    tracker
});

static mut DATA_BASE: Option<DataBaseInner> = None;

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
    /// When true, [`commit`](Self::commit) goes through [`DB::write_opt`] with
    /// `WriteOptions::disable_wal(true)`. Used by archive bulk-load (where
    /// data is replayable from the source on crash) to skip WAL append per
    /// commit.
    disable_wal: bool,
}

pub struct ArchiveRocksDBWriteBatch {
    inner: WriteBatch,
    account_writes: Vec<([u8; 64], Vec<u8>)>,
    storage_writes: Vec<([u8; 96], [u8; 32])>,
}

impl ArchiveRocksDBWriteBatch {
    fn new() -> Self {
        Self {
            inner: WriteBatch::default(),
            account_writes: Vec::new(),
            storage_writes: Vec::new(),
        }
    }

    /// Append pre-encoded account writes to the deferred cache. `None` is
    /// treated as a deletion (stored as empty bytes), matching the encoding
    /// used by [`StateDBWrite::write_account`].
    #[inline]
    pub fn extend_account_writes<I>(&mut self, items: I)
    where
        I: IntoIterator<Item = ([u8; 64], Option<Vec<u8>>)>,
    {
        self.account_writes
            .extend(items.into_iter().map(|(k, v)| (k, v.unwrap_or_default())));
    }

    /// Append pre-encoded storage writes to the deferred cache.
    #[inline]
    pub fn extend_storage_writes<I>(&mut self, items: I)
    where
        I: IntoIterator<Item = ([u8; 96], [u8; 32])>,
    {
        self.storage_writes.extend(items);
    }
}

impl Drop for DataBaseRef {
    fn drop(&mut self) {
        // Flush every named CF. See `DataBaseRef::flush` for why `db.flush()`
        // alone is insufficient.
        self.flush().unwrap();
        unsafe {
            DATA_BASE = None;
        }
    }
}

unsafe impl Send for DataBaseInner {}
unsafe impl Sync for DataBaseInner {}

/// Compression type for column families
#[derive(Clone, Copy)]
enum CfCompression {
    /// No compression (small values, not worth compressing)
    None,
    /// LZ4 compression (good balance of speed and compression)
    Lz4,
    /// ZSTD compression (high compression ratio for large values like code)
    Zstd,
}

#[inline]
fn rocksdb_column_options(
    shared_cache: &Cache,
    fixed_prefix_size: usize,
    disable_auto_compactions: bool,
    compression: CfCompression,
    bulk_load: bool,
) -> Options {
    let mut cf_opts = Options::default();
    cf_opts.set_max_total_wal_size(1 << 28); // e.g., 256MB
    cf_opts.set_keep_log_file_num(2);
    cf_opts.set_level_compaction_dynamic_level_bytes(true);
    if fixed_prefix_size != 0 {
        cf_opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(fixed_prefix_size));
        cf_opts.set_memtable_prefix_bloom_ratio(0.1);
    }
    let mut block_opts = BlockBasedOptions::default();

    // Use the shared cache for this column family
    block_opts.set_block_cache(shared_cache);
    block_opts.set_cache_index_and_filter_blocks(true);
    block_opts.set_pin_l0_filter_and_index_blocks_in_cache(true);
    block_opts.set_pin_top_level_index_and_filter(true);
    block_opts.set_bloom_filter(10.0, fixed_prefix_size != 0);
    block_opts.set_index_type(rocksdb::BlockBasedIndexType::TwoLevelIndexSearch);
    block_opts.set_partition_filters(true);
    block_opts.set_metadata_block_size(4096);
    cf_opts.set_block_based_table_factory(&block_opts);
    cf_opts.optimize_level_style_compaction(1 << 28); // e.g., 256MB
    cf_opts.set_max_compaction_bytes(2 * 1024 * 1024 * 1024); // 2GB
    // Allow the writer to roll into a fresh memtable while older ones are
    // still being flushed (default is 2). Helps under sustained write load
    // such as archive bulk-load.
    cf_opts.set_max_write_buffer_number(4);
    // Larger SST files = fewer files at every level = less metadata,
    // smaller index/bloom overhead, fewer files for compaction to track.
    // Default is 64MB; bump to 128MB.
    cf_opts.set_target_file_size_base(128 << 20);
    cf_opts.set_disable_auto_compactions(disable_auto_compactions);
    // Disable TTL-based compaction. Archive data is immutable — recompacting
    // old SST files just because they exceed 30 days (the default set by
    // optimize_level_style_compaction) provides no benefit and causes massive
    // temporary disk usage spikes (e.g. ~30 GB for AddressToStorage L6 rewrites).
    cf_opts.set_ttl(0);

    if bulk_load {
        // Disable every back-pressure mechanism on the writer thread. Auto
        // compaction still runs in the background to drain L0 → L1
        // incrementally, but if it can't keep up the writer must NOT be
        // throttled — archive ingest treats the source as replayable and
        // would rather pay the disk cost than slow down.
        // (Defaults: slowdown 20 files, stop 24 files, soft 64GB, hard 256GB.)
        cf_opts.set_level_zero_slowdown_writes_trigger(i32::MAX);
        cf_opts.set_level_zero_stop_writes_trigger(i32::MAX);
        cf_opts.set_soft_pending_compaction_bytes_limit(0);
        cf_opts.set_hard_pending_compaction_bytes_limit(0);
    }

    // Set compression based on data characteristics.
    // NOTE: optimize_level_style_compaction() populates compression_per_level
    // (L0-L1: None, L2+: LZ4), which takes precedence over set_compression_type().
    // We must use set_compression_per_level() to override it reliably.
    //
    // Under bulk_load auto-compaction is disabled and all writes pile up in L0
    // until the final manual compact(). With the default "L0/L1 = None" policy
    // that means the entire archive is held uncompressed on disk during ingest,
    // which can balloon to 2-3x of the post-compact footprint. Under bulk_load
    // we therefore compress L0/L1 too — flushes pay an extra LZ4/ZSTD encode,
    // but ingest doesn't read L0, so there is no latency cost.
    match compression {
        CfCompression::None => {
            cf_opts.set_compression_per_level(&[
                rocksdb::DBCompressionType::None,
                rocksdb::DBCompressionType::None,
                rocksdb::DBCompressionType::None,
                rocksdb::DBCompressionType::None,
                rocksdb::DBCompressionType::None,
                rocksdb::DBCompressionType::None,
                rocksdb::DBCompressionType::None,
            ]);
        }
        CfCompression::Lz4 => {
            let l0_l1 = if bulk_load {
                rocksdb::DBCompressionType::Lz4
            } else {
                // Hot read levels uncompressed in normal mode.
                rocksdb::DBCompressionType::None
            };
            cf_opts.set_compression_per_level(&[
                l0_l1,
                l0_l1,
                rocksdb::DBCompressionType::Lz4,
                rocksdb::DBCompressionType::Lz4,
                rocksdb::DBCompressionType::Lz4,
                rocksdb::DBCompressionType::Lz4,
                rocksdb::DBCompressionType::Lz4,
            ]);
        }
        CfCompression::Zstd => {
            // bulk_load: L0 = LZ4 (fast encode on flush), L1+ = ZSTD.
            // normal:    L0 = None (hot reads), L1 = LZ4, L2+ = ZSTD.
            let l0 = if bulk_load {
                rocksdb::DBCompressionType::Lz4
            } else {
                rocksdb::DBCompressionType::None
            };
            cf_opts.set_compression_per_level(&[
                l0,
                rocksdb::DBCompressionType::Lz4,
                rocksdb::DBCompressionType::Zstd,
                rocksdb::DBCompressionType::Zstd,
                rocksdb::DBCompressionType::Zstd,
                rocksdb::DBCompressionType::Zstd,
                rocksdb::DBCompressionType::Zstd,
            ]);
            // Enable ZSTD dictionary compression for better compression of similar data patterns
            // - max_train_bytes: bytes to sample for dictionary training (1MB)
            // - zstd_max_train_bytes: same as above for ZSTD specifically
            cf_opts.set_zstd_max_train_bytes(1024 * 1024); // 1MB training data
                                                           // compression_opts: (window_bits, level, strategy, max_dict_bytes)
                                                           // max_dict_bytes: dictionary size (16KB is a good default)
            cf_opts.set_compression_options(-14, 3, 0, 16 * 1024);
        }
    }

    cf_opts
}

#[inline]
fn rocksdb_options(disable_auto_compactions: bool) -> Options {
    let mut opts = Options::default();
    opts.create_missing_column_families(true);
    opts.create_if_missing(true);
    opts.set_use_fsync(false);
    opts.set_keep_log_file_num(1);
    // Larger sync chunks: amortise the per-fsync cost during heavy writes
    // (archive bulk-load and background compaction) at the small cost of a
    // slightly larger window of dirty pages on crash.
    opts.set_bytes_per_sync(4 << 20); // 4MB
    opts.set_write_buffer_size(1 << 28); // e.g., 256MB
    opts.set_max_bytes_for_level_base(1 << 28); // e.g., 256MB
    opts.set_max_total_wal_size(1 << 29); // e.g., 512MB
    // Background concurrency. The previous value (2) covered flush + a single
    // compaction job, which serialised L0 → L1 across CFs and bottlenecked
    // archive bulk-load; standalone reads/writes also benefit from faster
    // background compaction. `max_subcompactions` lets a single big
    // compaction (e.g. AddressToStorage) split into parallel ranges.
    opts.increase_parallelism(8);
    opts.set_max_subcompactions(4);
    opts.set_use_direct_io_for_flush_and_compaction(true);
    opts.set_disable_auto_compactions(disable_auto_compactions);

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
    /// Open the archive RocksDB. Set `disable_auto_compactions=true` for bulk
    /// ingest if you also plan to run a manual `compact()` at the end; this
    /// keeps the same WAL/throttle behavior as production reads.
    ///
    /// For archive-init (where data is replayable on crash), prefer
    /// [`Self::open_for_bulk_load`] which additionally disables WAL and the
    /// L0 / pending-compaction throttles.
    pub fn open<P: AsRef<Path>>(
        path: P,
        cache_size: usize,
        disable_auto_compactions: bool,
    ) -> Self {
        Self::open_inner(path, cache_size, disable_auto_compactions, false)
    }

    /// Open the archive RocksDB tuned for bulk ingest:
    /// - `disable_auto_compactions = false` — RocksDB drains L0 → L1 in the
    ///   background as files accumulate, so disk doesn't grow unbounded
    ///   during long ingests. The L0 slowdown/stop triggers and the
    ///   pending-compaction-byte limits are still disabled, so background
    ///   compaction never back-pressures the writer.
    /// - WAL disabled on `commit`.
    /// - L0/L1 compressed (see `rocksdb_column_options`) so the SSTs that
    ///   sit in L0 before auto-compaction picks them up are already small.
    ///
    /// Only safe when the source data is replayable on crash (e.g. archive
    /// ingest from S3). A crash before the final flush still loses every
    /// memtable that hasn't been flushed (no WAL fallback); resume relies on
    /// `latest_block_hash` rolling back to the last persisted checkpoint.
    /// The caller still runs the final `compact()` to consolidate into
    /// deeper levels with normal-mode compression.
    pub fn open_for_bulk_load<P: AsRef<Path>>(path: P, cache_size: usize) -> Self {
        warn!(
            target = "rocksdb",
            "Opening RocksDB archive in BULK-LOAD mode: WAL disabled, \
             L0/pending-compaction throttles disabled (auto compactions still on). \
             Crash before final compact() may lose writes since the last flush; \
             the source data must be replayable to recover."
        );
        // Note: disable_auto_compactions = false. Earlier revisions disabled
        // auto compactions and tried to do periodic manual compactions, but
        // every `compact_range_cf(.., None, None)` is a full-range manual
        // compaction (work proportional to total written so far), so periodic
        // calls produced O(N²) total compaction work. Letting RocksDB do
        // incremental L0 → L1 in the background keeps each step bounded.
        Self::open_inner(path, cache_size, false, true)
    }

    fn open_inner<P: AsRef<Path>>(
        path: P,
        cache_size: usize,
        disable_auto_compactions: bool,
        bulk_load: bool,
    ) -> Self {
        let total_cache_size = cache_size;
        let shared_cache = Cache::new_hyper_clock_cache(
            1024 * 1024 * total_cache_size,
            8192, // 8KB typical block size
        );
        info!(
            target = "rocksdb",
            "Created shared Clock Cache with size: {}MB for archive (bulk_load={})",
            total_cache_size,
            bulk_load,
        );

        // LatestBlockHash: single record, no compression needed
        let latest_block_hash_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::LatestBlockHash.to_str(),
            rocksdb_column_options(
                &shared_cache,
                0,
                disable_auto_compactions,
                CfCompression::None,
                bulk_load,
            ),
        );
        // BlockHashToBlockInfo: ~500 bytes value, LZ4 compression
        let block_hash_to_block_info_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::BlockHashToBlockInfo.to_str(),
            rocksdb_column_options(
                &shared_cache,
                0,
                disable_auto_compactions,
                CfCompression::Lz4,
                bulk_load,
            ),
        );
        // BlockNumToBlockHash: 32 bytes value, no compression needed
        let block_num_to_block_hash_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::BlockNumToBlockHash.to_str(),
            rocksdb_column_options(
                &shared_cache,
                0,
                disable_auto_compactions,
                CfCompression::None,
                bulk_load,
            ),
        );
        // AddressToAccount: ~100 bytes value, LZ4 compression
        let address_to_account_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToAccount.to_str(),
            rocksdb_column_options(
                &shared_cache,
                32,
                disable_auto_compactions,
                CfCompression::Lz4,
                bulk_load,
            ),
        );
        // AddressToStorage: 32 bytes value, but 96-byte keys have high prefix
        // redundancy. LZ4 at deeper levels reduces disk usage with negligible
        // CPU overhead.
        let address_to_storage_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToStorage.to_str(),
            rocksdb_column_options(
                &shared_cache,
                64,
                disable_auto_compactions,
                CfCompression::Lz4,
                bulk_load,
            ),
        );
        // HashToCode: large code blobs (KB~tens of KB), ZSTD for high compression
        let hash_to_code_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::HashToCode.to_str(),
            rocksdb_column_options(
                &shared_cache,
                0,
                disable_auto_compactions,
                CfCompression::Zstd,
                bulk_load,
            ),
        );
        let cfs = vec![
            latest_block_hash_cf,
            block_hash_to_block_info_cf,
            block_num_to_block_hash_cf,
            address_to_account_cf,
            address_to_storage_cf,
            hash_to_code_cf,
        ];
        let db_opt = rocksdb_options(disable_auto_compactions);
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
        unsafe { DATA_BASE = Some(DataBaseInner { _cols: cols, db }) }
        Self {
            db: unsafe { &DATA_BASE.as_ref().unwrap().db },
            disable_wal: bulk_load,
        }
    }
}

impl DataBaseRef {
    pub fn read_block_hash(&self, block_num: u64) -> Result<H256, Error> {
        let start = std::time::Instant::now();
        let block_num_to_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .unwrap();
        let block_num_bytes = encode_block_num(block_num);
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

    pub fn read_block_info(&self, block_hash: H256) -> Result<Option<BlockInfo>, Error> {
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
        let buf = &mut block_info_bytes.as_ref();
        let block_header = match RawHeader::decode(buf) {
            Ok(header) => header,
            Err(_) => {
                *buf = block_info_bytes.as_ref();
                let rlp_head = alloy_rlp::Header::decode(buf)?;
                if !rlp_head.list {
                    Err(alloy_rlp::Error::NonCanonicalSingleByte)?;
                }
                RawHeader {
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
                }
            }
        };
        // After RLP decode, remaining bytes (if any) are JSON-encoded OtherFields
        let other = if buf.is_empty() {
            Default::default()
        } else {
            serde_json::from_slice(buf).unwrap_or_default()
        };
        let block = Block {
            header: Header {
                hash: block_hash,
                inner: block_header,
                ..Default::default()
            },
            ..Default::default()
        };
        Ok(Some(BlockInfo {
            inner: block,
            other,
        }))
    }

    pub fn read_latest_block_hash(&self) -> Result<H256, Error> {
        let start = std::time::Instant::now();
        let latest_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
            .unwrap();
        let block_hash_bytes =
            self.db
                .get_cf_opt(latest_block_hash_cf, [1u8], &rocksdb_read_options())?;
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

    pub fn read_latest_block_num(&self) -> Result<Option<u64>, Error> {
        let latest_hash = self.read_latest_block_hash()?;
        if latest_hash == H256::ZERO {
            return Ok(None);
        }
        let block_info = self.read_block_info(latest_hash)?;
        match block_info {
            Some(block) => Ok(Some(block.header.number)),
            None => Ok(None),
        }
    }

    pub fn flush(&self) -> Result<(), Error> {
        // Flush every named column family. `DB::flush()` from rocksdb-rs only
        // flushes the default CF (which we don't use), so it is unsafe to rely
        // on for durability — especially under bulk-load (WAL disabled), where
        // unflushed memtable contents are not recoverable on crash.
        //
        // Ordering matters: `LatestBlockHash` is a pointer into the content
        // CFs and MUST flush last. If we crash mid-flush after content CFs
        // are persisted but before the pointer is, resume reads the older
        // pointer from SST and replays the lost block range (idempotent,
        // since archive keys include block_num). The reverse order risks
        // committing a pointer to a block whose content was lost with the
        // memtable.
        for cf_type in [
            StorageTypeColumn::BlockHashToBlockInfo,
            StorageTypeColumn::BlockNumToBlockHash,
            StorageTypeColumn::AddressToAccount,
            StorageTypeColumn::AddressToStorage,
            StorageTypeColumn::HashToCode,
            StorageTypeColumn::LatestBlockHash,
        ] {
            let cf = self.db.cf_handle(cf_type.to_str()).unwrap();
            self.db.flush_cf(cf)?;
        }
        Ok(())
    }

    pub fn compact(&self) -> Result<(), Error> {
        // Compact each column family separately to reduce memory usage
        let column_families = [
            StorageTypeColumn::LatestBlockHash,
            StorageTypeColumn::BlockHashToBlockInfo,
            StorageTypeColumn::BlockNumToBlockHash,
            StorageTypeColumn::AddressToAccount,
            StorageTypeColumn::AddressToStorage,
            StorageTypeColumn::HashToCode,
        ];

        for cf_type in column_families {
            let cf = self.db.cf_handle(cf_type.to_str()).unwrap();
            info!(target: "archive_compact", "Compacting column family: {}", cf_type);

            // For large column families, compact by key range (first byte prefix)
            // to further reduce memory usage
            match cf_type {
                StorageTypeColumn::AddressToAccount => {
                    // Key structure: address (32) || block_num (8) = 40 bytes
                    self.compact_cf_by_prefix::<40>(cf);
                }
                StorageTypeColumn::AddressToStorage => {
                    // Key structure: address (32) || index (32) || block_num (8) = 72 bytes
                    self.compact_cf_by_prefix::<72>(cf);
                }
                _ => {
                    // Small column families can be compacted in one go
                    self.db.compact_range_cf(cf, None::<&[u8]>, None::<&[u8]>);
                }
            }

            info!(target: "archive_compact", "Finished compacting column family: {}", cf_type);
        }

        Ok(())
    }

    /// Compact a column family in 16 ranges based on first byte's high nibble.
    /// This reduces memory usage by processing smaller chunks at a time.
    fn compact_cf_by_prefix<const KEY_LEN: usize>(&self, cf: &ColumnFamily) {
        for i in 0u8..16 {
            let start = [i << 4];
            let mut end = [0xFFu8; KEY_LEN];
            end[0] = (i << 4) | 0x0F;
            info!(target: "archive_compact", "  Range {}/16: 0x{:02X}-0x{:02X}",
                i + 1, i << 4, (i << 4) | 0x0F);
            self.db.compact_range_cf(cf, Some(&start), Some(&end));
        }
    }
}

impl LatestStateDBIterator for DataBaseRef {
    /// account address -> raw account
    /// Returns the latest state for each address (the record with highest block_num)
    fn account_iter(&self) -> impl Iterator<Item = Result<(H256, NewAccount), Error>> {
        // Data is sorted by address || block_num, so records for the same address are consecutive
        // and ordered by block_num ascending. We need the last record for each address.
        let mut iter = self
            .db
            .iterator_cf_opt(
                self.db
                    .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
                    .unwrap(),
                rocksdb_read_options(),
                IteratorMode::Start,
            )
            .peekable();

        std::iter::from_fn(move || {
            loop {
                let item = iter.next()?;
                if item.is_err() {
                    return Some(Err(Error::RocksDB(item.unwrap_err())));
                }
                let (key, value) = item.unwrap();
                let address_bytes: [u8; 32] = key[..32].try_into().unwrap();

                // Check if next record has the same address
                let is_last_for_address = match iter.peek() {
                    Some(Ok((next_key, _))) => next_key[..32] != address_bytes,
                    Some(Err(_)) => true, // Will handle error on next iteration
                    None => true,         // No more records
                };

                if !is_last_for_address {
                    // Skip this record, there's a newer one for this address
                    continue;
                }

                // This is the last (newest) record for this address
                let address = H256::from_slice(&address_bytes);
                let mut raw_account_slice = value.as_ref();

                // Skip empty values (deleted accounts)
                if raw_account_slice.is_empty() {
                    continue;
                }

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
                return Some(Ok((address, account)));
            }
        })
    }

    /// code hash -> code
    fn code_iter(&self) -> impl Iterator<Item = Result<(H256, Bytes), Error>> {
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
    /// Returns the latest state for each (address, index) pair (the record with highest block_num)
    fn storage_iter(&self) -> impl Iterator<Item = Result<(H256, H256, U256), Error>> {
        // Data is sorted by address || index || block_num, so records for the same (address, index)
        // are consecutive and ordered by block_num ascending. We need the last record for each pair.
        let mut iter = self
            .db
            .iterator_cf_opt(
                self.db
                    .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
                    .unwrap(),
                rocksdb_read_options(),
                IteratorMode::Start,
            )
            .peekable();

        std::iter::from_fn(move || {
            loop {
                let item = iter.next()?;
                if item.is_err() {
                    return Some(Err(Error::RocksDB(item.unwrap_err())));
                }
                let (key, value) = item.unwrap();
                let prefix: [u8; 64] = key[..64].try_into().unwrap(); // address || index

                // Check if next record has the same (address, index)
                let is_last_for_prefix = match iter.peek() {
                    Some(Ok((next_key, _))) => next_key[..64] != prefix,
                    Some(Err(_)) => true, // Will handle error on next iteration
                    None => true,         // No more records
                };

                if !is_last_for_prefix {
                    // Skip this record, there's a newer one for this (address, index)
                    continue;
                }

                // This is the last (newest) record for this (address, index)
                let address = H256::from_slice(&prefix[..32]);
                let storage_key = H256::from_slice(&prefix[32..64]);
                let storage_value = U256::from_be_slice(value.as_ref());

                // Skip zero values (deleted storage slots)
                if storage_value == U256::ZERO {
                    continue;
                }

                return Some(Ok((address, storage_key, storage_value)));
            }
        })
    }
}

impl BlockIterator for DataBaseRef {
    fn block_info_iter(&self) -> impl Iterator<Item = Result<BlockInfo, Error>> {
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
                let buf = &mut value.as_ref();
                let block_header: RawHeader = RawHeader::decode(buf).map_err(|e| {
                    Error::UnSupported(format!("Failed to decode block header: {}", e))
                })?;
                let other = if buf.is_empty() {
                    Default::default()
                } else {
                    serde_json::from_slice(buf).unwrap_or_default()
                };
                let block = Block {
                    header: Header {
                        hash: block_hash,
                        inner: block_header,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                Ok(BlockInfo {
                    inner: block,
                    other,
                })
            })
    }

    fn block_hash_iter(&self) -> impl Iterator<Item = Result<(u64, H256), Error>> {
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

impl StateDBProvider for Arc<DataBaseRef> {
    type StateDBReadWrite = StateDB;
    fn db_at(&self, block_id: BlockId) -> Result<Option<Self::StateDBReadWrite>, Error> {
        let block_num: u64;
        let cached_block_info: Option<BlockInfo>;
        match block_id {
            BlockId::Hash(hash) => {
                let info = self.read_block_info(hash.block_hash)?;
                if info.is_none() {
                    return Ok(None);
                }
                let info = info.unwrap();
                block_num = info.header.number;
                cached_block_info = Some(info);
            }
            BlockId::Number(block_number_or_tag) => match block_number_or_tag {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                    let latest_block_num = self.read_latest_block_num()?;
                    match latest_block_num {
                        Some(num) => {
                            block_num = num;
                            let block_hash = self.read_block_hash(num)?;
                            if block_hash == H256::ZERO {
                                return Ok(None);
                            }
                            let info = self.read_block_info(block_hash)?;
                            if info.is_none() {
                                return Ok(None);
                            }
                            cached_block_info = Some(info.unwrap());
                        }
                        None => {
                            block_num = 0;
                            cached_block_info = None;
                        }
                    }
                }
                BlockNumberOrTag::Number(num) => {
                    block_num = num;
                    let block_hash = self.read_block_hash(num)?;
                    if block_hash == H256::ZERO {
                        return Ok(None);
                    }
                    let info = self.read_block_info(block_hash)?;
                    if info.is_none() {
                        return Ok(None);
                    }
                    cached_block_info = Some(info.unwrap());
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

        // Create shared iterators
        let iterators = SharedIterators::new(account_iterator, storage_iterator);

        // Register with iterator tracker if enabled
        let (tracker_id, timed_out) = if ITERATOR_TRACKER.is_enabled() {
            let id = next_statedb_id();
            let flag = ITERATOR_TRACKER.register(id, block_num, iterators.clone());
            (Some(id), Some(flag))
        } else {
            (None, None)
        };

        Ok(Some(StateDB {
            db: self.clone(),
            block_num,
            block_info: cached_block_info,
            iterators,
            tracker_id,
            timed_out,
        }))
    }
}

pub struct StateDB {
    db: Arc<DataBaseRef>,
    block_num: u64,
    /// `None` when the database is empty (no blocks committed yet).
    /// Stores full BlockInfo to preserve `other` fields (e.g. l1FeeRate).
    block_info: Option<BlockInfo>,
    /// Shared iterators
    iterators: Arc<SharedIterators>,
    /// Tracker ID for iterator lifecycle management
    tracker_id: Option<u64>,
    /// Shared timeout flag with IteratorTracker
    timed_out: Option<TimeoutFlag>,
}

impl Clone for StateDB {
    fn clone(&self) -> Self {
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

        // Create shared iterators
        let iterators = SharedIterators::new(account_iterator, storage_iterator);

        // Register cloned StateDB with iterator tracker if enabled
        let (tracker_id, timed_out) = if ITERATOR_TRACKER.is_enabled() {
            let id = next_statedb_id();
            let flag = ITERATOR_TRACKER.register(id, self.block_num, iterators.clone());
            (Some(id), Some(flag))
        } else {
            (None, None)
        };

        Self {
            db: self.db.clone(),
            block_num: self.block_num,
            block_info: self.block_info.clone(),
            iterators,
            tracker_id,
            timed_out,
        }
    }
}

impl Debug for StateDB {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateDB")
            .field("block_num", &self.block_num)
            .field("block_info", &self.block_info)
            .finish()
    }
}

unsafe impl Send for StateDB {}
unsafe impl Sync for StateDB {}

impl Drop for StateDB {
    fn drop(&mut self) {
        // Unregister from iterator tracker if we have a tracker_id
        if let Some(id) = self.tracker_id {
            ITERATOR_TRACKER.unregister(id);
        }
    }
}

impl StateDB {
    /// Check if this StateDB's iterator has timed out
    #[inline]
    fn check_timeout(&self) -> Result<(), Error> {
        if let Some(ref flag) = self.timed_out {
            if flag.load(Ordering::Relaxed) {
                return Err(Error::IteratorTimedOut(self.block_num));
            }
        }
        Ok(())
    }
}

impl StateDBRead for StateDB {
    fn read_account(&self, address: H256) -> Result<Option<NewAccount>, Error> {
        let start = std::time::Instant::now();
        let address_bytes: [u8; 32] = address.into();
        let target_key = encode_account_key(address, self.block_num);

        // Check timeout before using iterator
        self.check_timeout()?;
        let mut account_iter_guard = self.iterators.account_iterator.lock().unwrap();
        let account_iter = account_iter_guard
            .as_mut()
            .ok_or(Error::IteratorTimedOut(self.block_num))?;
        account_iter.seek_for_prev(target_key);
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
        let target_key = encode_storage_key(address, key, self.block_num);

        // Check timeout before using iterator
        self.check_timeout()?;
        let mut storage_iter_guard = self.iterators.storage_iterator.lock().unwrap();
        let storage_iter = storage_iter_guard
            .as_mut()
            .ok_or(Error::IteratorTimedOut(self.block_num))?;
        storage_iter.seek_for_prev(target_key);
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

    fn read_block_hash(&self, block_num: u64) -> Result<H256, Error> {
        if let Some(info) = &self.block_info {
            if block_num == self.block_num {
                return Ok(info.header.hash);
            }
        }
        self.db.read_block_hash(block_num)
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<BlockInfo>, Error> {
        if let Some(info) = &self.block_info {
            if block_hash == info.header.hash {
                return Ok(Some(info.clone()));
            }
        }
        self.db.read_block_info(block_hash)
    }
    fn read_latest_block_hash(&self) -> Result<H256, Error> {
        match &self.block_info {
            Some(info) => Ok(info.header.hash),
            None => self.db.read_latest_block_hash(),
        }
    }
}
/// StateDB delegates all write operations to Arc<DataBaseRef>
impl StateDBWrite for StateDB {
    type DBWriteBatch = ArchiveRocksDBWriteBatch;

    fn prepare_write_batch(&self) -> Result<Self::DBWriteBatch, Error> {
        self.db.prepare_write_batch()
    }

    fn write_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_num: u64,
        block_hash: H256,
    ) -> Result<(), Error> {
        self.db.write_block_hash(batch, block_num, block_hash)
    }

    fn write_block_info(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_info: BlockInfo,
    ) -> Result<(), Error> {
        self.db.write_block_info(batch, block_info)
    }

    fn write_account(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        block_num: u64,
        raw_account: Option<NewAccount>,
    ) -> Result<(), Error> {
        self.db
            .write_account(batch, address, block_num, raw_account)
    }

    fn write_storage(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        key: H256,
        block_num: u64,
        value: U256,
    ) -> Result<(), Error> {
        self.db.write_storage(batch, address, key, block_num, value)
    }

    fn write_code(
        &self,
        batch: &mut Self::DBWriteBatch,
        code_hash: H256,
        code: Bytes,
    ) -> Result<(), Error> {
        self.db.write_code(batch, code_hash, code)
    }

    fn write_latest_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_hash: H256,
    ) -> Result<(), Error> {
        self.db.write_latest_block_hash(batch, block_hash)
    }

    fn commit(&self, batch: Self::DBWriteBatch) -> Result<(), Error> {
        self.db.commit(batch)
    }
}

/// Direct write implementation for `Arc<DataBaseRef>` without needing a StateDB instance.
/// This is useful for bulk initialization where we don't need read capabilities.
impl StateDBWrite for Arc<DataBaseRef> {
    type DBWriteBatch = ArchiveRocksDBWriteBatch;

    fn prepare_write_batch(&self) -> Result<Self::DBWriteBatch, Error> {
        Ok(ArchiveRocksDBWriteBatch::new())
    }

    fn write_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_num: u64,
        block_hash: H256,
    ) -> Result<(), Error> {
        let block_num_to_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .unwrap();
        let block_hash_bytes: [u8; 32] = block_hash.into();
        let block_num_bytes = encode_block_num(block_num);
        batch.inner.put_cf(
            block_num_to_block_hash_cf,
            block_num_bytes,
            block_hash_bytes,
        );
        Ok(())
    }

    fn write_block_info(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_info: BlockInfo,
    ) -> Result<(), Error> {
        let block_hash_to_block_info_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
            .unwrap();
        let block_hash_bytes: [u8; 32] = block_info.header.hash.into();
        let mut block_info_bytes = Vec::new();
        let block_header: RawHeader = block_info.header.inner.clone();
        block_header.encode(&mut block_info_bytes);
        if !block_info.other.is_empty() {
            let other_bytes = serde_json::to_vec(&block_info.other).map_err(|e| {
                Error::UnSupported(format!("Failed to serialize other fields: {}", e))
            })?;
            block_info_bytes.extend_from_slice(&other_bytes);
        }
        batch.inner.put_cf(
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
        let account_key = encode_account_key(address, block_num);
        let value = raw_account.map(encode_slim_account).unwrap_or_default();
        batch.account_writes.push((account_key, value));
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
        let storage_key = encode_storage_key(address, key, block_num);
        let value_bytes: [u8; 32] = value.to_be_bytes();

        batch.storage_writes.push((storage_key, value_bytes));
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
        batch
            .inner
            .put_cf(address_to_code_cf, code_hash_bytes, code);
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
        batch
            .inner
            .put_cf(latest_block_hash_cf, [1u8], block_hash.as_slice());
        Ok(())
    }

    fn commit(&self, mut batch: Self::DBWriteBatch) -> Result<(), Error> {
        if !batch.account_writes.is_empty() {
            let address_to_account_cf = self
                .db
                .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
                .unwrap();
            // Stable sort by key: equal keys keep insertion order, so the last
            // write per key is naturally the last entry within its run.
            batch.account_writes.sort_by(|a, b| a.0.cmp(&b.0));

            let mut last_write: Option<([u8; 64], Vec<u8>)> = None;
            for (key, value) in batch.account_writes {
                if let Some((prev_key, prev_value)) = last_write.take() {
                    if prev_key != key {
                        batch
                            .inner
                            .put_cf(address_to_account_cf, prev_key, prev_value);
                    }
                }
                last_write = Some((key, value));
            }
            if let Some((key, value)) = last_write {
                batch.inner.put_cf(address_to_account_cf, key, value);
            }
        }

        if !batch.storage_writes.is_empty() {
            let address_to_storage_cf = self
                .db
                .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
                .unwrap();
            // Stable sort by key: equal keys keep insertion order, so the last
            // write per key is naturally the last entry within its run.
            batch.storage_writes.sort_by(|a, b| a.0.cmp(&b.0));

            let mut last_write: Option<([u8; 96], [u8; 32])> = None;
            for (key, value) in batch.storage_writes {
                if let Some((prev_key, prev_value)) = last_write.take() {
                    if prev_key != key {
                        batch
                            .inner
                            .put_cf(address_to_storage_cf, prev_key, prev_value);
                    }
                }
                last_write = Some((key, value));
            }
            if let Some((key, value)) = last_write {
                batch.inner.put_cf(address_to_storage_cf, key, value);
            }
        }

        if self.disable_wal {
            let mut wo = WriteOptions::default();
            wo.disable_wal(true);
            self.db.write_opt(batch.inner, &wo)?;
        } else {
            self.db.write(batch.inner)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::serde::OtherFields;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn make_block_info_with_other() -> BlockInfo {
        let block_json = json!({
            "hash":"0x321040cad932841b60186b937dd7b05a9a19a7b5a4d9f34e6f824f3b98346c5c",
            "parentHash":"0xcc4374d84f296ed5b45631fdafc7b788ea6070080c8d2a39447229af5fe779b5",
            "sha3Uncles":"0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
            "miner":"0x3100000000000000000000000000000000000005",
            "stateRoot":"0x8c0c93b3d3e3fbc8f39151adf14d6beee4cf1b2268f6ed0da6043ad3c8505cff",
            "transactionsRoot":"0xad8a61086a9fb2b79c10e248dcedef3d2d07082e518ca04ab966d4b5e6df6684",
            "receiptsRoot":"0xd7382684c788e295ee56dfd7fc0503becf28766585400cee47ce3eb1b790e4d8",
            "logsBloom":"0x00000000000000000000000000000000000000000000000000400000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000800000000000080000000000000000000000000000000000400000000000008400000000000000000002000000000000000400000000000140000000000000002000000000000000000000000040000000010000000002000000000004000000000000000000000000000000000000000000000400000000000000000000000000080000000000000000000000000000000000440000000000000000000000000000000020000000000000000000000000000000000000000",
            "difficulty":"0x0",
            "number":"0x1",
            "gasLimit":"0x989680",
            "gasUsed":"0x5a444",
            "timestamp":"0x692608fb",
            "extraData":"0x",
            "mixHash":"0x0000000000000000000000000000000000000000000000000000000000000000",
            "nonce":"0x0000000000000000",
            "baseFeePerGas":"0x342770c0",
            "withdrawalsRoot":"0x56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421",
            "blobGasUsed":"0x0",
            "excessBlobGas":"0x0",
            "parentBeaconBlockRoot":"0x0000000000000000000000000000000000000000000000000000000000000000",
            "requestsHash":"0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "l1FeeRate":"0xdf847580"
        });
        serde_json::from_value::<BlockInfo>(block_json).unwrap()
    }

    /// Simulate the write_block_info encoding: RLP(RawHeader) + JSON(other)
    fn encode_block_info(block_info: &BlockInfo) -> Vec<u8> {
        let mut bytes = Vec::new();
        let raw_header: RawHeader = block_info.header.inner.clone();
        raw_header.encode(&mut bytes);
        if !block_info.other.is_empty() {
            let other_bytes = serde_json::to_vec(&block_info.other).unwrap();
            bytes.extend_from_slice(&other_bytes);
        }
        bytes
    }

    /// Simulate the read_block_info decoding: RLP decode + JSON other
    fn decode_block_info_bytes(
        block_hash: H256,
        data: &[u8],
    ) -> Result<BlockInfo, Box<dyn std::error::Error>> {
        let buf = &mut &data[..];
        let block_header = RawHeader::decode(buf)?;
        let other: OtherFields = if buf.is_empty() {
            Default::default()
        } else {
            serde_json::from_slice(buf).unwrap_or_default()
        };
        let block = Block {
            header: Header {
                hash: block_hash,
                inner: block_header,
                ..Default::default()
            },
            ..Default::default()
        };
        Ok(BlockInfo {
            inner: block,
            other,
        })
    }

    #[test]
    fn test_archive_block_info_roundtrip_preserves_other_fields() {
        let original = make_block_info_with_other();

        assert!(
            !original.other.is_empty(),
            "test data should have other fields"
        );
        assert!(
            original.other.get("l1FeeRate").is_some(),
            "test data should have l1FeeRate"
        );

        let encoded = encode_block_info(&original);
        let decoded =
            decode_block_info_bytes(original.header.hash, &encoded).expect("decode should succeed");

        assert_eq!(decoded.header.inner.number, original.header.inner.number);
        assert_eq!(
            decoded.header.inner.base_fee_per_gas,
            original.header.inner.base_fee_per_gas
        );
        assert_eq!(
            decoded.header.inner.requests_hash,
            original.header.inner.requests_hash
        );

        assert!(
            !decoded.other.is_empty(),
            "decoded block should preserve other fields"
        );
        assert_eq!(
            decoded.other.get("l1FeeRate"),
            original.other.get("l1FeeRate"),
            "l1FeeRate should survive roundtrip"
        );
    }

    #[test]
    fn test_archive_block_info_roundtrip_without_other_fields() {
        let block = Block {
            header: Header {
                hash: H256::ZERO,
                inner: RawHeader {
                    number: 42,
                    gas_limit: 10_000_000,
                    base_fee_per_gas: Some(1_000_000_000),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let original = BlockInfo::new(block);
        assert!(original.other.is_empty());

        let encoded = encode_block_info(&original);
        let decoded = decode_block_info_bytes(H256::ZERO, &encoded).expect("decode should succeed");

        assert_eq!(decoded.header.inner.number, 42);
        assert_eq!(decoded.header.inner.gas_limit, 10_000_000);
        assert!(decoded.other.is_empty());
    }

    #[test]
    fn test_cached_block_info_preserves_other_fields() {
        let original = make_block_info_with_other();
        let cached = Some(original.clone());

        let block_hash = original.header.hash;
        let result = cached
            .as_ref()
            .filter(|info| info.header.hash == block_hash)
            .cloned();

        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(
            result.other.get("l1FeeRate"),
            original.other.get("l1FeeRate"),
            "cached block_info should preserve l1FeeRate"
        );
    }

    #[test]
    fn test_block_info_iter_decode_preserves_other_fields() {
        let original = make_block_info_with_other();
        let encoded = encode_block_info(&original);

        let buf = &mut encoded.as_slice();
        let block_header: RawHeader = RawHeader::decode(buf).unwrap();
        let other: OtherFields = if buf.is_empty() {
            Default::default()
        } else {
            serde_json::from_slice(buf).unwrap_or_default()
        };
        let block = Block {
            header: Header {
                hash: original.header.hash,
                inner: block_header,
                ..Default::default()
            },
            ..Default::default()
        };
        let decoded = BlockInfo {
            inner: block,
            other,
        };

        assert_eq!(
            decoded.other.get("l1FeeRate"),
            original.other.get("l1FeeRate"),
            "iter decode should preserve l1FeeRate"
        );
    }

    #[test]
    fn test_multiple_other_fields_roundtrip() {
        let block = Block {
            header: Header {
                hash: H256::ZERO,
                inner: RawHeader {
                    number: 100,
                    base_fee_per_gas: Some(875_000_000),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let mut other_map = BTreeMap::new();
        other_map.insert(
            "l1FeeRate".to_string(),
            serde_json::Value::String("0xdf847580".to_string()),
        );
        other_map.insert(
            "customField".to_string(),
            serde_json::Value::String("0xdeadbeef".to_string()),
        );
        let original = BlockInfo {
            inner: block,
            other: serde_json::from_value(serde_json::to_value(&other_map).unwrap()).unwrap(),
        };

        let encoded = encode_block_info(&original);
        let decoded = decode_block_info_bytes(H256::ZERO, &encoded).expect("decode should succeed");

        assert_eq!(
            decoded.other.get("l1FeeRate"),
            original.other.get("l1FeeRate")
        );
        assert_eq!(
            decoded.other.get("customField"),
            original.other.get("customField")
        );
    }
}
