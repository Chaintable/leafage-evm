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
    inverted_block_encoding,
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
    BlockBasedOptions, Cache, ColumnFamily, ColumnFamilyDescriptor, IngestExternalFileOptions,
    IteratorMode, Options, ReadOptions, SliceTransform, SstFileWriter, WriteBatch, WriteOptions,
    DB,
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
}

pub struct ArchiveRocksDBWriteBatch {
    inner: WriteBatch,
    account_writes: Vec<([u8; 64], Vec<u8>)>,
    storage_writes: Vec<([u8; 96], [u8; 32])>,
    /// When true, the underlying `write_opt` is called with `sync=true`,
    /// forcing fsync of the WAL segment for this batch before returning.
    /// Used by archive ingest at checkpoint boundaries to make the resume
    /// pointer durable without paying for fsync on every commit.
    sync: bool,
}

impl ArchiveRocksDBWriteBatch {
    fn new() -> Self {
        Self {
            inner: WriteBatch::default(),
            account_writes: Vec::new(),
            storage_writes: Vec::new(),
            sync: false,
        }
    }

    /// Mark this batch as a sync commit: fsync the WAL before `commit()`
    /// returns. Without this, the WAL append is buffered and a power loss
    /// could lose the most recent commits.
    pub fn set_sync(&mut self, sync: bool) {
        self.sync = sync;
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
    /// No compression — only used for tiny CFs where the encode CPU isn't worth it.
    None,
    /// LZ4 across all levels. Default policy for medium-sized CFs.
    Lz4,
    /// LZ4 at shallow levels, ZSTD with dict at deep levels. Best ratio for code-like blobs.
    Zstd,
}

#[inline]
fn rocksdb_column_options(
    shared_cache: &Cache,
    fixed_prefix_size: usize,
    disable_auto_compactions: bool,
    compression: CfCompression,
    bulk_load: bool,
    archive_zstd_compression: bool,
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
        // Push the L0 → L1 compaction trigger from the default 4 files up to
        // 50 so background compaction stops competing with the WAL writer
        // for disk bandwidth on slower NVMes. Observed on a ~150 MB/s NVMe:
        // ingest at 1024-block checkpoints had compaction reading ~40 MB/s
        // and writing ~30 MB/s while the WAL writer needed ~250 MB/checkpoint
        // → checkpoint stalls 14s. Letting L0 grow to 50 files frees disk
        // bandwidth for the WAL/flush path; the deferred compaction work is
        // picked up by the final manual compact() the caller runs at end of
        // ingest.
        cf_opts.set_level_zero_file_num_compaction_trigger(50);
    }

    // Compression policy.
    //
    // Two policies coexist, gated by `archive_zstd_compression`:
    //
    // * `archive_zstd_compression = false` (default — preserves pre-branch
    //   behavior for existing deployments):
    //   - Lz4: L0/L1 = None in normal mode, LZ4 in bulk-load; L2-L6 = LZ4.
    //   - Zstd: L0 = None in normal mode, LZ4 in bulk-load; L1 = LZ4; L2-L6 = ZSTD-with-dict.
    //
    // * `archive_zstd_compression = true` (opt-in — newer policy from this branch):
    //   - Lz4: LZ4 across all levels unconditionally.
    //   - Zstd: L0-L1 = LZ4 unconditionally; L2-L6 = ZSTD-with-dict.
    //   The unconditional shallow-level compression fixes a bug where a
    //   bulk-loaded DB reopened in normal mode would have its L0 SSTs
    //   (written as LZ4) rewritten as None on cascade, inflating disk by
    //   ~1.7× until the data reached the next compressed level. LZ4
    //   decompresses at ~4 GB/s — comparable to NVMe sequential read — so
    //   shallow compression has negligible read-latency cost.
    //
    // NOTE on dynamic levels: this CF enables level_compaction_dynamic_level_bytes,
    // so physical level numbers below the flush level shift with DB size. What
    // matters is that the flush output (compression_per_level[0]) and all deeper
    // levels reachable via cascade are set consistently — they are.
    //
    // NOTE on overrides: optimize_level_style_compaction() populates compression_per_level
    // (L0-L1: None, L2+: LZ4), which takes precedence over set_compression_type().
    // We must use set_compression_per_level() to override it reliably.
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
            let l0_l1 = if archive_zstd_compression || bulk_load {
                rocksdb::DBCompressionType::Lz4
            } else {
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
            let l0 = if archive_zstd_compression || bulk_load {
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
            cf_opts.set_zstd_max_train_bytes(1024 * 1024);
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
    /// Open the archive RocksDB with default throttles. Set
    /// `disable_auto_compactions=true` if a caller plans to run its own
    /// manual `compact()` and doesn't want background work in between.
    ///
    /// For archive-init bulk ingest, prefer [`Self::open_for_bulk_load`]
    /// which disables the L0 / pending-compaction throttles so the writer
    /// is never back-pressured by lagging compaction.
    pub fn open<P: AsRef<Path>>(
        path: P,
        cache_size: usize,
        disable_auto_compactions: bool,
        archive_zstd_compression: bool,
    ) -> Self {
        Self::open_inner(
            path,
            cache_size,
            disable_auto_compactions,
            false,
            archive_zstd_compression,
        )
    }

    /// Open the archive RocksDB tuned for bulk ingest:
    /// - Auto-compactions ON: RocksDB drains L0 → L1 in the background as
    ///   files accumulate, so disk doesn't grow unbounded during long
    ///   ingests.
    /// - L0 slowdown/stop triggers and pending-compaction byte limits
    ///   disabled: background compaction never back-pressures the writer.
    /// - WAL is on (same as `open()`); callers wanting durable resume
    ///   should mark each checkpoint batch as `set_sync(true)` so the WAL
    ///   is fsynced before the commit returns.
    pub fn open_for_bulk_load<P: AsRef<Path>>(
        path: P,
        cache_size: usize,
        archive_zstd_compression: bool,
    ) -> Self {
        warn!(
            target = "rocksdb",
            "Opening RocksDB archive in BULK-LOAD mode: \
             L0/pending-compaction throttles disabled (auto compactions still on, \
             WAL still on). Caller is expected to drive durability by calling \
             commit() with sync=true on checkpoint batches."
        );
        // Note: disable_auto_compactions = false. Earlier revisions disabled
        // auto compactions and tried to do periodic manual compactions, but
        // every `compact_range_cf(.., None, None)` is a full-range manual
        // compaction (work proportional to total written so far), so periodic
        // calls produced O(N²) total compaction work. Letting RocksDB do
        // incremental L0 → L1 in the background keeps each step bounded.
        Self::open_inner(path, cache_size, false, true, archive_zstd_compression)
    }

    fn open_inner<P: AsRef<Path>>(
        path: P,
        cache_size: usize,
        disable_auto_compactions: bool,
        bulk_load: bool,
        archive_zstd_compression: bool,
    ) -> Self {
        let total_cache_size = cache_size;
        let shared_cache = Cache::new_hyper_clock_cache(
            1024 * 1024 * total_cache_size,
            8192, // 8KB typical block size
        );
        info!(
            target = "rocksdb",
            "Created shared Clock Cache with size: {}MB for archive (bulk_load={}, archive_zstd_compression={})",
            total_cache_size,
            bulk_load,
            archive_zstd_compression,
        );

        // The three large CFs (BlockHashToBlockInfo, AddressToAccount,
        // AddressToStorage) use Lz4 by default. Opt into Zstd-with-dict at
        // deep levels via `archive_zstd_compression` for ~15-20% extra ratio
        // at the cost of ~2× compaction CPU and ~3× cold-read decode latency.
        // HashToCode is always Zstd: code blobs benefit from dict compression
        // regardless of the flag.
        let big_cf_compression = if archive_zstd_compression {
            CfCompression::Zstd
        } else {
            CfCompression::Lz4
        };

        // LatestBlockHash: single record, no compression needed
        let latest_block_hash_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::LatestBlockHash.to_str(),
            rocksdb_column_options(
                &shared_cache,
                0,
                disable_auto_compactions,
                CfCompression::None,
                bulk_load,
                archive_zstd_compression,
            ),
        );
        let block_hash_to_block_info_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::BlockHashToBlockInfo.to_str(),
            rocksdb_column_options(
                &shared_cache,
                0,
                disable_auto_compactions,
                big_cf_compression,
                bulk_load,
                archive_zstd_compression,
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
                archive_zstd_compression,
            ),
        );
        let address_to_account_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToAccount.to_str(),
            rocksdb_column_options(
                &shared_cache,
                32,
                disable_auto_compactions,
                big_cf_compression,
                bulk_load,
                archive_zstd_compression,
            ),
        );
        let address_to_storage_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToStorage.to_str(),
            rocksdb_column_options(
                &shared_cache,
                64,
                disable_auto_compactions,
                big_cf_compression,
                bulk_load,
                archive_zstd_compression,
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
                archive_zstd_compression,
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

    /// SstFileWriter Options used for archive bulk-load. Compression matches the
    /// L0/L1 setting of the large CFs (LZ4) so RocksDB does not transcode the
    /// ingested file when cascading. The default BytewiseComparator matches the
    /// CF comparator. Block-table layout is left default; bloom/index are
    /// regenerated by the final manual `compact()` when files cascade out of L0.
    fn sst_writer_options() -> Options {
        let mut opts = Options::default();
        opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
        opts
    }

    /// Default IngestExternalFileOptions for archive bulk-load. `move_files=true`
    /// renames the file into RocksDB's data directory instead of copying — the
    /// caller is responsible for ensuring the source path is on the same
    /// filesystem as the database. Other options are left at RocksDB defaults
    /// (`snapshot_consistency=true`, `allow_global_seqno=true`,
    /// `allow_blocking_flush=true`, `ingest_behind=false`); these are correct
    /// for the bulk-load workload, where we do not write to the target CFs'
    /// memtables and depend on global seqno assignment for correct read-merge
    /// of any duplicate keys produced by crash-resume re-ingest.
    fn ingest_external_file_options() -> IngestExternalFileOptions {
        let mut opts = IngestExternalFileOptions::default();
        opts.set_move_files(true);
        opts
    }

    /// Build an SST file at `path` containing the `AddressToAccount` writes.
    ///
    /// REQUIRES: `sorted_writes` is strictly increasing by key (per
    /// BytewiseComparator). Caller must sort and (defensively) dedup-keep-last
    /// before calling. Empty `sorted_writes` produces no file (the SST writer
    /// rejects empty finishes); caller must guard.
    pub fn write_account_sst<P: AsRef<Path>>(
        &self,
        path: P,
        sorted_writes: &[([u8; 64], Vec<u8>)],
    ) -> Result<(), Error> {
        let opts = Self::sst_writer_options();
        let mut writer = SstFileWriter::create(&opts);
        writer.open(path.as_ref())?;
        for (k, v) in sorted_writes {
            writer.put(k, v)?;
        }
        writer.finish()?;
        Ok(())
    }

    /// Build an SST file at `path` containing the `AddressToStorage` writes.
    /// Same preconditions as [`Self::write_account_sst`].
    pub fn write_storage_sst<P: AsRef<Path>>(
        &self,
        path: P,
        sorted_writes: &[([u8; 96], [u8; 32])],
    ) -> Result<(), Error> {
        let opts = Self::sst_writer_options();
        let mut writer = SstFileWriter::create(&opts);
        writer.open(path.as_ref())?;
        for (k, v) in sorted_writes {
            writer.put(k, v)?;
        }
        writer.finish()?;
        Ok(())
    }

    /// Ingest a list of pre-built SST files into the `AddressToAccount` CF.
    /// Files in `paths` must have non-overlapping key ranges among themselves
    /// (otherwise they all land in L0 anyway). Across calls the key ranges may
    /// overlap (and will, because address space is sparse but block_num
    /// progresses), so files cascade through L0.
    pub fn ingest_account_ssts(&self, paths: Vec<std::path::PathBuf>) -> Result<(), Error> {
        let cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
            .unwrap();
        let opts = Self::ingest_external_file_options();
        self.db.ingest_external_file_cf_opts(cf, &opts, paths)?;
        Ok(())
    }

    /// Ingest a list of pre-built SST files into the `AddressToStorage` CF.
    pub fn ingest_storage_ssts(&self, paths: Vec<std::path::PathBuf>) -> Result<(), Error> {
        let cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
            .unwrap();
        let opts = Self::ingest_external_file_options();
        self.db.ingest_external_file_cf_opts(cf, &opts, paths)?;
        Ok(())
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
        // Records for the same address are consecutive. The newest version is
        // the FIRST record of the prefix under inverted (newest-first) encoding,
        // and the LAST record under legacy ascending encoding.
        let inverted = inverted_block_encoding();
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

        // Address prefix whose newest version has already been handled (inverted).
        let mut consumed_prefix: Option<[u8; 32]> = None;

        std::iter::from_fn(move || {
            loop {
                let item = iter.next()?;
                if item.is_err() {
                    return Some(Err(Error::RocksDB(item.unwrap_err())));
                }
                let (key, value) = item.unwrap();
                let address_bytes: [u8; 32] = key[..32].try_into().unwrap();

                let is_newest = if inverted {
                    // First record of the prefix is newest.
                    let newest = consumed_prefix != Some(address_bytes);
                    consumed_prefix = Some(address_bytes);
                    newest
                } else {
                    // Last record of the prefix is newest.
                    match iter.peek() {
                        Some(Ok((next_key, _))) => next_key[..32] != address_bytes,
                        Some(Err(_)) => true,
                        None => true,
                    }
                };
                if !is_newest {
                    continue;
                }

                let mut raw_account_slice = value.as_ref();
                // Newest version is a deletion -> account absent at tip.
                if raw_account_slice.is_empty() {
                    continue;
                }

                let address = H256::from_slice(&address_bytes);
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
        // Records for the same (address, index) are consecutive. The newest is
        // the FIRST record of the prefix under inverted encoding, the LAST under
        // legacy ascending encoding.
        let inverted = inverted_block_encoding();
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

        // (address || index) prefix whose newest has been handled (inverted).
        let mut consumed_prefix: Option<[u8; 64]> = None;

        std::iter::from_fn(move || {
            loop {
                let item = iter.next()?;
                if item.is_err() {
                    return Some(Err(Error::RocksDB(item.unwrap_err())));
                }
                let (key, value) = item.unwrap();
                let prefix: [u8; 64] = key[..64].try_into().unwrap(); // address || index

                let is_newest = if inverted {
                    let newest = consumed_prefix != Some(prefix);
                    consumed_prefix = Some(prefix);
                    newest
                } else {
                    match iter.peek() {
                        Some(Ok((next_key, _))) => next_key[..64] != prefix,
                        Some(Err(_)) => true,
                        None => true,
                    }
                };
                if !is_newest {
                    continue;
                }

                let storage_value = U256::from_be_slice(value.as_ref());
                // Newest version is zero -> slot empty at tip.
                if storage_value == U256::ZERO {
                    continue;
                }

                let address = H256::from_slice(&prefix[..32]);
                let storage_key = H256::from_slice(&prefix[32..64]);
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
        // Greatest version <= block_num: inverted encoding -> smallest key >=
        // target (forward seek); legacy ascending -> largest key <= target
        // (seek_for_prev). See archive_encoding module docs.
        if inverted_block_encoding() {
            account_iter.seek(target_key);
        } else {
            account_iter.seek_for_prev(target_key);
        }
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
        // Inverted -> forward seek; legacy ascending -> seek_for_prev. See read_account.
        if inverted_block_encoding() {
            storage_iter.seek(target_key);
        } else {
            storage_iter.seek_for_prev(target_key);
        }
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

        let mut wo = WriteOptions::default();
        wo.set_sync(batch.sync);
        self.db.write_opt(batch.inner, &wo)?;
        Ok(())
    }
}

/// Serializes tests that call [`DataBaseRef::open`]. The archive backend keeps
/// the open DB in a process-global `static mut DATA_BASE` and hands out a
/// pointer into it, so two concurrent opens invalidate each other's reference.
/// Every test that opens an archive DB must hold this lock for its duration.
#[cfg(test)]
pub(crate) static ARCHIVE_DB_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

#[cfg(test)]
mod rewind_tests {
    use super::*;
    use crate::db::StateDBWrapper;
    use crate::interface::EvmStorageWrite;
    use leafage_evm_types::{AccountStorageDiff, BlockStorageDiff, IndexValuePair};

    fn make_block_info_at(number: u64, hash: H256, parent_hash: H256) -> BlockInfo {
        let mut raw = RawHeader::default();
        raw.number = number;
        raw.parent_hash = parent_hash;
        BlockInfo::new(Block {
            header: Header {
                hash,
                inner: raw,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn make_diff(addr: H256, slot: H256, balance: u64, nonce: u64, value: u64) -> BlockStorageDiff {
        BlockStorageDiff {
            new_accounts: vec![NewAccount {
                address: addr,
                balance: U256::from(balance),
                nonce,
                code_hash: KECCAK256_EMPTY.0.into(),
            }],
            storage_diffs: vec![AccountStorageDiff {
                address: addr,
                diffs: vec![IndexValuePair {
                    index: slot,
                    value: U256::from(value),
                }],
            }],
            ..Default::default()
        }
    }

    /// Archive-mode counterpart of the snapshot rewind invariant test: an
    /// empty-diff `update_block` must move the committed-head pointer
    /// without touching the height-versioned state, reads pinned at the
    /// rewound head must see that height's values (not the newer ones),
    /// and replaying the skipped diff must converge back to the old head.
    #[test]
    fn test_empty_diff_update_block_rewinds_head_pointer_archive() {
        let _g = super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "leafage-archive-rewind-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let db = Arc::new(DataBaseRef::open(&dir, 64, false, false));
            let latest = |db: &Arc<DataBaseRef>| {
                StateDBWrapper(
                    db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                        .unwrap()
                        .unwrap(),
                )
            };

            let addr = H256::repeat_byte(0xaa);
            let slot = H256::repeat_byte(0x01);

            let block1 = make_block_info_at(1, H256::repeat_byte(0x11), H256::ZERO);
            latest(&db)
                .update_block(block1.clone(), make_diff(addr, slot, 100, 1, 7))
                .unwrap();
            let block2 = make_block_info_at(2, H256::repeat_byte(0x22), block1.header.hash);
            let diff2 = make_diff(addr, slot, 200, 2, 9);
            latest(&db).update_block(block2.clone(), diff2.clone()).unwrap();

            // Rewind the head pointer to block 1 with an empty diff.
            latest(&db)
                .update_block(block1.clone(), BlockStorageDiff::default())
                .unwrap();
            let state = latest(&db);
            let head = state.last_committed_block().unwrap().unwrap();
            assert_eq!(head.header.number, 1);
            assert_eq!(head.header.hash, block1.header.hash);

            // A handle pinned at the rewound head sees block-1 values...
            let account = state.0.read_account(addr).unwrap().unwrap();
            assert_eq!(account.balance, U256::from(100));
            assert_eq!(account.nonce, 1);
            assert_eq!(state.0.read_storage(addr, slot).unwrap(), U256::from(7));

            // ...while the block-2 versions remain queryable as history.
            let state_at_2 = StateDBWrapper(
                db.db_at(BlockId::Number(BlockNumberOrTag::Number(2)))
                    .unwrap()
                    .unwrap(),
            );
            let account = state_at_2.0.read_account(addr).unwrap().unwrap();
            assert_eq!(account.balance, U256::from(200));
            assert_eq!(state_at_2.0.read_storage(addr, slot).unwrap(), U256::from(9));

            // Replaying block 2's diff converges back to the old head.
            latest(&db).update_block(block2.clone(), diff2).unwrap();
            let state = latest(&db);
            let head = state.last_committed_block().unwrap().unwrap();
            assert_eq!(head.header.number, 2);
            assert_eq!(head.header.hash, block2.header.hash);
            let account = state.0.read_account(addr).unwrap().unwrap();
            assert_eq!(account.balance, U256::from(200));
            assert_eq!(state.0.read_storage(addr, slot).unwrap(), U256::from(9));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod inverted_encoding_tests {
    use super::*;
    use crate::db::{LatestStateDBIterator, StateDBRead, StateDBWrapper};
    use crate::interface::EvmStorageWrite;
    use leafage_evm_types::{AccountStorageDiff, BlockStorageDiff, IndexValuePair};

    fn block_info(number: u64) -> BlockInfo {
        let mut raw = RawHeader::default();
        raw.number = number;
        BlockInfo::new(Block {
            header: Header {
                hash: H256::with_last_byte(number as u8),
                inner: raw,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn slot_diff(addr: H256, slot: H256, balance: u64, value: u64) -> BlockStorageDiff {
        BlockStorageDiff {
            new_accounts: vec![NewAccount {
                address: addr,
                balance: U256::from(balance),
                nonce: 1,
                code_hash: KECCAK256_EMPTY.0.into(),
            }],
            storage_diffs: vec![AccountStorageDiff {
                address: addr,
                diffs: vec![IndexValuePair {
                    index: slot,
                    value: U256::from(value),
                }],
            }],
            ..Default::default()
        }
    }

    /// End-to-end versioned read/write round-trip, parameterized by encoding
    /// mode. Writing a slot at blocks 5/10/20 and reading at arbitrary heights
    /// must return the greatest version <= H, absence below the first write,
    /// and the latest-state iterators must surface the newest version — under
    /// BOTH the legacy ascending and the inverted descending encodings.
    fn run_versioned_roundtrip(inverted: bool) {
        let _g = super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::db_impl::archive_encoding::set_inverted_block_encoding(inverted);
        let dir = std::env::temp_dir().join(format!(
            "leafage-archive-enc-{}-{}",
            inverted,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let db = Arc::new(DataBaseRef::open(&dir, 64, false, false));
            let addr = H256::repeat_byte(0xab);
            let slot = H256::repeat_byte(0x01);

            // Contiguous blocks 1..=20; the slot/account change only at 5/10/20.
            for n in 1..=20u64 {
                let diff = match n {
                    5 => slot_diff(addr, slot, 100, 7),
                    10 => slot_diff(addr, slot, 200, 9),
                    20 => slot_diff(addr, slot, 300, 11),
                    _ => BlockStorageDiff::default(),
                };
                let state = StateDBWrapper(
                    db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                        .unwrap()
                        .unwrap(),
                );
                state.update_block(block_info(n), diff).unwrap();
            }

            let at = |n: u64| {
                db.db_at(BlockId::Number(BlockNumberOrTag::Number(n)))
                    .unwrap()
                    .unwrap()
            };

            // Below the first write: slot empty, account absent.
            assert_eq!(at(4).read_storage(addr, slot).unwrap(), U256::ZERO);
            assert!(at(4).read_account(addr).unwrap().is_none());

            // greatest version <= H at and between change points.
            for (h, val, bal) in [
                (5u64, 7u64, 100u64),
                (7, 7, 100),
                (9, 7, 100),
                (10, 9, 200),
                (15, 9, 200),
                (19, 9, 200),
                (20, 11, 300),
            ] {
                assert_eq!(
                    at(h).read_storage(addr, slot).unwrap(),
                    U256::from(val),
                    "storage at height {h} (inverted={inverted})"
                );
                assert_eq!(
                    at(h).read_account(addr).unwrap().unwrap().balance,
                    U256::from(bal),
                    "balance at height {h} (inverted={inverted})"
                );
            }

            // Latest-state iterators surface the newest version.
            let storages: Vec<_> = db.storage_iter().map(|r| r.unwrap()).collect();
            assert_eq!(storages, vec![(addr, slot, U256::from(11))]);
            let accounts: Vec<_> = db.account_iter().map(|r| r.unwrap()).collect();
            assert_eq!(accounts.len(), 1);
            assert_eq!(accounts[0].0, addr);
            assert_eq!(accounts[0].1.balance, U256::from(300));
        }
        let _ = std::fs::remove_dir_all(&dir);
        // Restore the default so other tests aren't affected.
        crate::db_impl::archive_encoding::set_inverted_block_encoding(false);
    }

    #[test]
    fn test_versioned_read_greatest_leq_height_inverted() {
        run_versioned_roundtrip(true);
    }

    #[test]
    fn test_versioned_read_greatest_leq_height_legacy() {
        run_versioned_roundtrip(false);
    }
}
