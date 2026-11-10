//! Archive schema v3: account and storage versions keyed by RocksDB
//! user-defined timestamps. See `docs/ArchiveSchemaV3.md`.
//!
//! The versioned CFs keep their names but store the key alone (address, or
//! address ‖ slot) with the block as a u64 timestamp, compared by RocksDB's
//! built-in `leveldb.BytewiseComparator.u64ts` (newest first). A read at
//! height `h` is one point `Get` with read timestamp `h`, which returns the
//! greatest version `<= h` and its block. Every other CF is unchanged.
//!
//! The comparator is fixed when a CF is created, so an archive declares the
//! schema in `<db>/LEAFAGE_ARCHIVE_SCHEMA` and is opened with the matching
//! options; RocksDB itself refuses a comparator that does not match.

use super::block_index::{self, copy_block_infos, BlockIndex};
use super::{
    archive_cf_descriptors, archive_cf_options, decode_archive_header, decode_storage,
    encode_block_num, existing_columns, fmt_hms, key_after_prefix, rocksdb_options, shard_bounds,
    DataBaseRef, StorageTypeColumn, ENCODING_MARKER_KEY,
};
use crate::db_impl::error::Error;
use crate::db_impl::rocksdb_impl::sst::{ingest_external_file_options, SstSink};
use crate::metrics::STORAGE_METRICS;
use leafage_evm_types::{H256, U256};
use librocksdb_sys as ffi;
use rocksdb::{
    AsColumnFamilyRef, AsRawPtr, Cache, ColumnFamilyDescriptor, IteratorMode, Options, ReadOptions,
    DB,
};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use tracing::{info, warn};

/// File in the database directory naming the archive schema.
pub(crate) const SCHEMA_FILE: &str = "LEAFAGE_ARCHIVE_SCHEMA";
const SCHEMA_V3: &str = "3";
const COMPARATOR: &str = "leveldb.BytewiseComparator.u64ts";
/// Versioned SST files roll at key boundaries past this size, the CFs'
/// `target_file_size_base`.
const ROLL_BYTES: u64 = 128 << 20;
/// Threads of the rewind trim's compactions (the serving archive uses 4).
const TRIM_SUBCOMPACTIONS: u32 = 8;
/// Batch only enough cache misses to amortize MultiGet's setup allocations.
pub(super) const MIN_MULTIGET_KEYS: usize = 8;

/// Whether the archive at `path` is schema v3.
pub(crate) fn is_udt_archive(path: &Path) -> Result<bool, Error> {
    match std::fs::read_to_string(path.join(SCHEMA_FILE)) {
        Ok(s) if s.trim() == SCHEMA_V3 => Ok(true),
        Ok(s) => Err(Error::UnSupported(format!(
            "unknown archive schema {:?} in {SCHEMA_FILE}",
            s.trim()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// The on-disk layout of a new RocksDB archive's account and storage
/// history (see `docs/ArchiveSchemaV3.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveSchema {
    /// Ascending 32-byte block tail.
    V1,
    /// Descending block tail (inverted encoding).
    V2,
    /// Block as a RocksDB user-defined timestamp.
    V3,
}

impl ArchiveSchema {
    /// Whether the versioned keys use the inverted block encoding. A v3
    /// archive records the inverted marker too: its write path encodes keys
    /// as v2 does before splitting off the block.
    pub fn inverted(self) -> bool {
        self != Self::V1
    }
}

impl std::str::FromStr for ArchiveSchema {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "v1" => Ok(Self::V1),
            "v2" => Ok(Self::V2),
            "v3" => Ok(Self::V3),
            _ => Err(format!("unknown archive schema {s:?} (v1, v2 or v3)")),
        }
    }
}

impl std::fmt::Display for ArchiveSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
            Self::V3 => "v3",
        })
    }
}

/// Lay out `path` for a new RocksDB archive of `schema`, if no database is
/// there yet: create the directory and, for v3, the schema file, so the open
/// that follows creates v3 column families. Returns whether the database is
/// new; an existing one keeps its own schema and is left untouched.
pub fn init_archive_schema(path: &Path, schema: ArchiveSchema) -> Result<bool, Error> {
    if path.join("CURRENT").exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(path)?;
    if schema == ArchiveSchema::V3 {
        write_schema_file(path)?;
    } else {
        // Left by an earlier start that asked for v3 and stopped before
        // RocksDB created the database.
        match std::fs::remove_file(path.join(SCHEMA_FILE)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(true)
}

fn write_schema_file(path: &Path) -> Result<(), Error> {
    let tmp = path.join(format!("{SCHEMA_FILE}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(format!("{SCHEMA_V3}\n").as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path.join(SCHEMA_FILE))?;
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

/// Options of a versioned CF before the archive settings: the u64
/// timestamp comparator. rust-rocksdb only takes a Rust callback comparator,
/// so the built-in one is loaded by name.
pub(crate) fn base_cf_options() -> Options {
    Options::default()
        .get_options_from_string(format!("comparator={COMPARATOR}"))
        .expect("RocksDB's built-in u64 timestamp comparator")
}

/// The timestamp of a version written at `block` (fixed64, as the
/// comparator decodes it).
#[inline]
pub(crate) fn encode_ts(block: u64) -> [u8; 8] {
    block.to_le_bytes()
}

/// Minimal big-endian form of a storage value: zero is empty.
#[inline]
pub(crate) fn strip_leading_zeros(value: &[u8]) -> &[u8] {
    let start = value.iter().position(|&b| b != 0).unwrap_or(value.len());
    &value[start..]
}

fn take_error(err: *mut c_char) -> Error {
    // SAFETY: a non-null error from the C API is a malloc'ed C string.
    let msg = unsafe { CStr::from_ptr(err) }
        .to_string_lossy()
        .into_owned();
    unsafe { ffi::rocksdb_free(err as *mut c_void) };
    Error::RocksDBMessage(msg)
}

/// Read options at one height for timestamped point lookups. rust-rocksdb
/// has no `Get` that returns the timestamp of the version found, so this
/// owns a C `rocksdb_readoptions_t` and calls the C API.
pub(super) struct UdtReader {
    ro: *mut ffi::rocksdb_readoptions_t,
    /// The read timestamp; the C options point into it.
    _ts: Box<[u8; 8]>,
}

/// First version in (block, head], bounded to this key and read view.
/// Timestamp-range iteration exposes all versions, newest first, so
/// SeekForPrev finds the oldest one in the range. The pinned RocksDB exposes
/// internal keys in this mode: user key || timestamp (8) || sequence/type (8).
pub(super) fn next_version(
    db: &DB,
    col: StorageTypeColumn,
    key: &[u8],
    block: u64,
    head: u64,
) -> Result<Option<u64>, Error> {
    debug_assert!(block < head);
    let mut ro = ReadOptions::default();
    ro.set_verify_checksums(false);
    ro.set_total_order_seek(true);
    ro.set_timestamp(encode_ts(head));
    ro.set_iter_start_ts(encode_ts(block + 1));
    ro.set_iterate_lower_bound(key.to_vec());
    if let Some(end) = key_after_prefix(key) {
        ro.set_iterate_upper_bound(end);
    }
    let cf = db.cf_handle(col.to_str()).unwrap();
    let mut iter = db.raw_iterator_cf_opt(cf, ro);
    iter.seek_for_prev(key);
    iter.status()?;
    let Some(raw) = iter.key() else {
        return Ok(None);
    };
    if raw.len() != key.len() + 16 || &raw[..key.len()] != key {
        return Err(Error::RocksDBMessage(
            "unexpected timestamp-range iterator key".into(),
        ));
    }
    let next = u64::from_le_bytes(raw[key.len()..key.len() + 8].try_into().unwrap());
    if next <= block || next > head {
        return Err(Error::RocksDBMessage(
            "timestamp-range iterator outside read view".into(),
        ));
    }
    Ok(Some(next))
}

// SAFETY: the options are only read after construction; RocksDB reads them
// concurrently from any thread.
unsafe impl Send for UdtReader {}
unsafe impl Sync for UdtReader {}

impl UdtReader {
    pub(super) fn new(block: u64) -> Self {
        let ts = Box::new(encode_ts(block));
        // SAFETY: `ts` is heap-allocated and outlives `ro` (dropped after it).
        let ro = unsafe {
            let ro = ffi::rocksdb_readoptions_create();
            ffi::rocksdb_readoptions_set_verify_checksums(ro, 0);
            ffi::rocksdb_readoptions_set_timestamp(ro, ts.as_ptr() as *const c_char, ts.len());
            ro
        };
        Self { ro, _ts: ts }
    }

    /// The greatest version `<= h` of `key` in `col`.
    pub(super) fn get(
        &self,
        db: &DB,
        col: StorageTypeColumn,
        key: &[u8],
    ) -> Result<Option<Found>, Error> {
        let cf = db.cf_handle(col.to_str()).unwrap();
        let (mut len, mut ts, mut ts_len) = (0usize, std::ptr::null_mut(), 0usize);
        let mut err: *mut c_char = std::ptr::null_mut();
        // SAFETY: all pointers are valid for the call; outputs are malloc'ed.
        let value = unsafe {
            ffi::rocksdb_get_cf_with_ts(
                db.as_raw_ptr(),
                self.ro,
                cf.inner(),
                key.as_ptr() as *const c_char,
                key.len(),
                &mut len,
                &mut ts,
                &mut ts_len,
                &mut err,
            )
        };
        // SAFETY: these are the owned outputs of the C call above.
        let found = unsafe { Found::take(value, len, ts, ts_len, err) };
        count_gets(col, std::slice::from_ref(&found));
        found
    }

    /// Same versions and input order as `get`, including empty values.
    pub(super) fn get_many(
        &self,
        db: &DB,
        col: StorageTypeColumn,
        keys: &[&[u8]],
    ) -> Result<Vec<Option<Found>>, Error> {
        if keys.len() < MIN_MULTIGET_KEYS {
            return keys.iter().map(|key| self.get(db, col, key)).collect();
        }
        let cf = db.cf_handle(col.to_str()).unwrap();
        let n = keys.len();
        let cfs = vec![cf.inner() as *const ffi::rocksdb_column_family_handle_t; n];
        let pointers: Vec<_> = keys
            .iter()
            .map(|key| key.as_ptr().cast::<c_char>())
            .collect();
        let lengths: Vec<_> = keys.iter().map(|key| key.len()).collect();
        let mut values = vec![std::ptr::null_mut(); n];
        let mut sizes = vec![0; n];
        let mut timestamps = vec![std::ptr::null_mut(); n];
        let mut timestamp_sizes = vec![0; n];
        let mut errors = vec![std::ptr::null_mut(); n];
        // SAFETY: all arrays have n entries; options and keys outlive the call.
        unsafe {
            ffi::rocksdb_multi_get_cf_with_ts(
                db.as_raw_ptr(),
                self.ro,
                cfs.as_ptr(),
                n,
                pointers.as_ptr(),
                lengths.as_ptr(),
                values.as_mut_ptr(),
                sizes.as_mut_ptr(),
                timestamps.as_mut_ptr(),
                timestamp_sizes.as_mut_ptr(),
                errors.as_mut_ptr(),
            );
        }
        // Own/free EVERY C result before propagating an error: collecting
        // directly into Result would leak the outputs after the first error.
        let owned: Vec<_> = (0..n)
            .map(|i| unsafe {
                Found::take(
                    values[i],
                    sizes[i],
                    timestamps[i],
                    timestamp_sizes[i],
                    errors[i],
                )
            })
            .collect();
        count_gets(col, &owned);
        owned.into_iter().collect()
    }
}

/// Count the versioned reads that found a version at or below the read
/// height, and those that found none.
fn count_gets(col: StorageTypeColumn, results: &[Result<Option<Found>, Error>]) {
    let m = &*STORAGE_METRICS;
    let (found, missing) = match col {
        StorageTypeColumn::AddressToAccount => (
            &m.archive_account_gets_found,
            &m.archive_account_gets_missing,
        ),
        _ => (
            &m.archive_storage_gets_found,
            &m.archive_storage_gets_missing,
        ),
    };
    let hits = results.iter().filter(|r| matches!(r, Ok(Some(_)))).count();
    let misses = results.iter().filter(|r| matches!(r, Ok(None))).count();
    found.increment(hits as u64);
    missing.increment(misses as u64);
}

impl Drop for UdtReader {
    fn drop(&mut self) {
        // SAFETY: created by `rocksdb_readoptions_create`, destroyed once.
        unsafe { ffi::rocksdb_readoptions_destroy(self.ro) }
    }
}

/// A version returned by [`UdtReader::get`].
pub(super) struct Found {
    value: *mut c_char,
    len: usize,
    /// Block that wrote the value.
    pub block: u64,
}

impl Found {
    /// Take ownership of the malloc'ed outputs of a timestamped C read.
    unsafe fn take(
        value: *mut c_char,
        len: usize,
        ts: *mut c_char,
        ts_len: usize,
        err: *mut c_char,
    ) -> Result<Option<Self>, Error> {
        let mut found = Self {
            value,
            len,
            block: 0,
        };
        if !err.is_null() {
            ffi::rocksdb_free(ts.cast());
            return Err(take_error(err));
        }
        // Found iff a timestamp came back: empty values can be null malloc(0).
        if ts_len == 0 {
            ffi::rocksdb_free(ts.cast());
            return Ok(None);
        }
        // The versioned CF comparator always returns an eight-byte timestamp.
        found.block =
            u64::from_le_bytes(std::slice::from_raw_parts(ts.cast(), 8).try_into().unwrap());
        ffi::rocksdb_free(ts.cast());
        Ok(Some(found))
    }

    pub(super) fn value(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `value` holds `len` bytes until dropped.
        unsafe { std::slice::from_raw_parts(self.value as *const u8, self.len) }
    }
}

impl Drop for Found {
    fn drop(&mut self) {
        if !self.value.is_null() {
            // SAFETY: malloc'ed by the C API, freed once.
            unsafe { ffi::rocksdb_free(self.value as *mut c_void) }
        }
    }
}

/// The non-zero slots of `address` at `block`: a timestamped iterator yields
/// each slot once, at its greatest version `<= block`.
pub(crate) fn storage_slots(
    db: &DB,
    address: [u8; 32],
    block: u64,
) -> Result<Vec<(H256, U256)>, Error> {
    let cf = db
        .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
        .unwrap();
    let mut ro = ReadOptions::default();
    ro.set_verify_checksums(false);
    ro.set_timestamp(encode_ts(block));
    ro.set_iterate_lower_bound(address.to_vec());
    if let Some(upper) = key_after_prefix(&address) {
        ro.set_iterate_upper_bound(upper);
    }
    let mut iter = db.raw_iterator_cf_opt(cf, ro);
    iter.seek(address);
    let mut slots = Vec::new();
    while let Some(key) = iter.key() {
        let value = decode_storage(iter.value().unwrap());
        if !value.is_zero() {
            slots.push((H256::from_slice(&key[32..64]), value));
        }
        iter.next();
    }
    iter.status()?;
    Ok(slots)
}

// ---------------------------------------------------------------------------
// Rewind
// ---------------------------------------------------------------------------

/// Check, on a read-only open, that block `number` is in the v3 archive at
/// `path` and not above its head, before [`trim_history`] drops what is
/// above it.
pub(crate) fn check_rewind_target(path: &Path, number: u64) -> Result<(), Error> {
    let db = open_read_only(path, true)?;
    let get = |col: StorageTypeColumn, key: &[u8]| -> Result<Option<Vec<u8>>, Error> {
        Ok(db.get_cf(db.cf_handle(col.to_str()).unwrap(), key)?)
    };
    let latest = get(StorageTypeColumn::LatestBlockHash, &[1u8])?;
    let target_hash = get(
        StorageTypeColumn::BlockNumToBlockHash,
        &encode_block_num(number),
    )?;
    let (head, target) = if db
        .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
        .is_some()
    {
        let header_number = |hash: Option<Vec<u8>>| -> Result<Option<u64>, Error> {
            let Some(hash) = hash else { return Ok(None) };
            get(StorageTypeColumn::BlockHashToBlockInfo, &hash)?
                .map(|raw| Ok(decode_archive_header(&mut &raw[..])?.number))
                .transpose()
        };
        (header_number(latest)?, header_number(target_hash)?)
    } else {
        // Block infos by number, each starting with its hash. The head is
        // the latest one with the head's hash: a bulk load may have written
        // blocks past it.
        let cf = db
            .cf_handle(StorageTypeColumn::BlockNumToBlockInfo.to_str())
            .unwrap();
        let mut head = None;
        if let Some(latest) = &latest {
            for item in db.iterator_cf(cf, IteratorMode::End) {
                let (key, record) = item?;
                if record.starts_with(latest) {
                    head = Some(u64::from_be_bytes(key[..].try_into().unwrap()));
                    break;
                }
            }
        }
        let record = get(
            StorageTypeColumn::BlockNumToBlockInfo,
            &number.to_be_bytes(),
        )?;
        let target = match (target_hash, record) {
            (Some(hash), Some(record)) if record.starts_with(&hash) => Some(number),
            _ => None,
        };
        (head, target)
    };
    match (head, target) {
        (Some(head), Some(target)) if target == number && number <= head => Ok(()),
        _ => Err(Error::UnSupported(format!(
            "archive block {number} is missing or above the committed head"
        ))),
    }
}

/// Drop every account and storage version above block `number`, with
/// RocksDB's trim-on-open: a full compaction of CFs `4` and `5` keeping the
/// timestamps `<= number`. The other CFs have no timestamps and are left to
/// the caller.
pub(crate) fn trim_history(path: &Path, number: u64) -> Result<(), Error> {
    let cache = Cache::new_hyper_clock_cache(256 << 20, 8192);
    let mut db_opts = rocksdb_options(false);
    db_opts.create_if_missing(false);
    db_opts.create_missing_column_families(false);
    // The trim is one manual compaction at a time, each split into this many
    // threads (spawned per compaction, outside the background pool).
    db_opts.set_max_subcompactions(TRIM_SUBCOMPACTIONS);
    // RocksDB opens every CF of the database, `default` included.
    let mut names = vec![CString::new("default").unwrap()];
    let mut cf_opts = vec![Options::default()];
    for col in existing_columns(path).unwrap_or_default() {
        names.push(CString::new(col.to_str()).unwrap());
        let mut opts = archive_cf_options(col, &cache, false, false, false, true);
        // The trim's bottommost pass skips files numbered after it started
        // (kForceOptimized): an automatic compaction running meanwhile would
        // write untrimmed files it never visits.
        opts.set_disable_auto_compactions(true);
        cf_opts.push(opts);
    }
    let name_ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
    // SAFETY: the options outlive the call, which copies them.
    let opt_ptrs: Vec<*const ffi::rocksdb_options_t> = cf_opts
        .iter()
        .map(|o| unsafe { o.as_raw_ptr() } as *const _)
        .collect();
    let mut handles = vec![std::ptr::null_mut(); names.len()];
    let mut ts = encode_ts(number);
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| Error::UnSupported(format!("invalid archive path: {e}")))?;
    let mut err: *mut c_char = std::ptr::null_mut();
    let start = std::time::Instant::now();
    info!(target: "rewind", number, "trimming v3 account and storage history (full compaction)");
    // SAFETY: every pointer is valid for the call; on success the database
    // and its handles are released right after.
    unsafe {
        let db = ffi::rocksdb_open_and_trim_history(
            db_opts.as_raw_ptr(),
            c_path.as_ptr(),
            names.len() as c_int,
            name_ptrs.as_ptr(),
            opt_ptrs.as_ptr(),
            handles.as_mut_ptr(),
            ts.as_mut_ptr() as *mut c_char,
            ts.len(),
            &mut err,
        );
        if !err.is_null() {
            return Err(take_error(err));
        }
        for handle in handles {
            ffi::rocksdb_column_family_handle_destroy(handle);
        }
        ffi::rocksdb_close(db);
    }
    info!(target: "rewind", number, elapsed = %fmt_hms(start.elapsed().as_secs_f64()), "v3 history trimmed");
    Ok(())
}

// ---------------------------------------------------------------------------
// Offline conversion v2 -> v3
// ---------------------------------------------------------------------------

/// Records and bytes written by [`DataBaseRef::convert_inverted_to_udt`].
#[derive(Debug, Default, Clone)]
pub struct UdtConvertReport {
    pub accounts: u64,
    pub storage: u64,
    pub other: u64,
    pub files: usize,
}

enum Task {
    /// Copy a non-versioned CF verbatim.
    Copy(StorageTypeColumn),
    /// Move the block infos of `BlockHashToBlockInfo` into `BlockNumToBlockInfo`.
    BlockInfos,
    /// Convert one key-range shard of a versioned CF.
    Versioned {
        col: StorageTypeColumn,
        shard: usize,
        lo: u8,
        hi: Option<u8>,
    },
}

fn open_read_only(path: &Path, udt: bool) -> Result<DB, Error> {
    let mut opts = Options::default();
    opts.set_max_open_files(
        std::env::var("ROCKSDB_MAX_OPEN_FILE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(512),
    );
    let cols = existing_columns(path).unwrap_or_default();
    let descriptors = cols.into_iter().map(|col| {
        let versioned = matches!(
            col,
            StorageTypeColumn::AddressToAccount | StorageTypeColumn::AddressToStorage
        );
        let cf_opts = if udt && versioned {
            base_cf_options()
        } else {
            Options::default()
        };
        ColumnFamilyDescriptor::new(col.to_str(), cf_opts)
    });
    Ok(DB::open_cf_descriptors_read_only(
        &opts,
        path,
        descriptors,
        false,
    )?)
}

fn scan_options(lo: Option<u8>, hi: Option<u8>) -> ReadOptions {
    let mut ro = ReadOptions::default();
    ro.set_verify_checksums(false);
    ro.set_total_order_seek(true);
    ro.set_readahead_size(16 << 20);
    ro.fill_cache(false);
    if let Some(lo) = lo {
        ro.set_iterate_lower_bound(vec![lo]);
    }
    if let Some(hi) = hi {
        ro.set_iterate_upper_bound(vec![hi]);
    }
    ro
}

impl DataBaseRef {
    /// Offline rewrite of a v2 (inverted) archive into schema v3 at
    /// `dst_path`, which must not exist. The source is opened read-only.
    ///
    /// v2 sorts a key's versions newest first, the order of the timestamp
    /// comparator, so each shard streams straight into SST files: the key
    /// without its 32-byte tail, the block as the timestamp, storage values
    /// without leading zeros. Files roll only between keys, so no two files
    /// share a key and all of them ingest into the bottom level. They are
    /// written with the CFs' own table options (filters, partitioned index),
    /// so no compaction follows. Block infos are copied as they are, or with
    /// `ARCHIVE_BLOCK_INDEX=1` go to `BlockNumToBlockInfo`.
    pub fn convert_inverted_to_udt(
        src_path: &Path,
        dst_path: &Path,
        cache_size: usize,
        jobs: usize,
    ) -> Result<UdtConvertReport, Error> {
        if dst_path.exists() && std::fs::read_dir(dst_path)?.next().is_some() {
            return Err(Error::UnSupported(format!(
                "destination {} is not empty",
                dst_path.display()
            )));
        }
        let jobs = jobs.clamp(1, 64);
        let src = open_read_only(src_path, false)?;
        let marker = src
            .get_cf(
                src.cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
                    .unwrap(),
                ENCODING_MARKER_KEY,
            )?
            .and_then(|v| v.first().copied());
        if marker != Some(1) {
            return Err(Error::UnSupported(
                "source must be a v2 archive (encoding marker = inverted)".into(),
            ));
        }

        let src_cols = existing_columns(src_path).unwrap_or_default();
        // Block infos move from CF 2 to CF 7 only when asked for.
        let move_infos =
            block_index::enabled() && src_cols.contains(&StorageTypeColumn::BlockHashToBlockInfo);
        let cols: Vec<StorageTypeColumn> = if move_infos {
            src_cols
                .iter()
                .copied()
                .filter(|&col| col != StorageTypeColumn::BlockHashToBlockInfo)
                .chain(
                    (!src_cols.contains(&StorageTypeColumn::BlockNumToBlockInfo))
                        .then_some(StorageTypeColumn::BlockNumToBlockInfo),
                )
                .collect()
        } else {
            src_cols.clone()
        };
        let cache = Cache::new_hyper_clock_cache(cache_size << 20, 8192);
        let mut db_opts = rocksdb_options(false);
        db_opts.set_error_if_exists(true);
        let dst = DB::open_cf_descriptors(
            &db_opts,
            dst_path,
            archive_cf_descriptors(&cache, false, false, false, true, &cols),
        )?;
        let tmp_dir = dst_path.join(".udt_tmp");
        std::fs::create_dir_all(&tmp_dir)?;
        // SST writers use the destination CFs' options: same comparator,
        // filters, index and bottom-level compression.
        let sst_opts: Vec<(StorageTypeColumn, Options)> = cols
            .iter()
            .map(|&col| {
                (
                    col,
                    archive_cf_options(col, &cache, false, false, false, true),
                )
            })
            .collect();
        let opts_of = |col: StorageTypeColumn| &sst_opts.iter().find(|(c, _)| *c == col).unwrap().1;

        // The block info move first, as it runs on one thread, then the
        // storage shards: they are the bulk of the work.
        let mut tasks = Vec::new();
        if move_infos {
            tasks.push(Task::BlockInfos);
        }
        for col in [
            StorageTypeColumn::AddressToStorage,
            StorageTypeColumn::AddressToAccount,
        ] {
            for (shard, (lo, hi)) in shard_bounds(4 * jobs).into_iter().enumerate() {
                tasks.push(Task::Versioned { col, shard, lo, hi });
            }
        }
        for &col in &src_cols {
            let moved = move_infos && col == StorageTypeColumn::BlockHashToBlockInfo;
            if !moved
                && !matches!(
                    col,
                    StorageTypeColumn::AddressToStorage | StorageTypeColumn::AddressToAccount
                )
            {
                tasks.push(Task::Copy(col));
            }
        }
        let n_tasks = tasks.len();
        let queue = Mutex::new(tasks.into_iter());
        let done_tasks = AtomicU64::new(0);
        let records = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
        let bytes_read = AtomicU64::new(0);
        let stop = AtomicBool::new(false);
        let outputs: Mutex<Vec<(StorageTypeColumn, usize, Vec<PathBuf>)>> = Mutex::new(Vec::new());
        let start = std::time::Instant::now();

        std::thread::scope(|scope| -> Result<(), Error> {
            scope.spawn(|| {
                let mut ticks = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    ticks += 1;
                    if !ticks.is_multiple_of(30) {
                        continue;
                    }
                    let secs = start.elapsed().as_secs_f64();
                    let read = bytes_read.load(Ordering::Relaxed);
                    info!(target: "migrate",
                        "udt convert: {}/{} tasks, {} storage + {} account + {} other records, \
                         {:.1} GiB read ({:.0} MiB/s), elapsed {}",
                        done_tasks.load(Ordering::Relaxed), n_tasks,
                        records[0].load(Ordering::Relaxed), records[1].load(Ordering::Relaxed),
                        records[2].load(Ordering::Relaxed), read as f64 / (1u64 << 30) as f64,
                        read as f64 / (1u64 << 20) as f64 / secs, fmt_hms(secs));
                }
            });
            let workers: Vec<_> = (0..jobs)
                .map(|_| {
                    scope.spawn(|| -> Result<(), Error> {
                        loop {
                            let Some(task) = queue.lock().unwrap().next() else {
                                return Ok(());
                            };
                            if stop.load(Ordering::Relaxed) {
                                return Ok(());
                            }
                            let (col, shard, paths) = match task {
                                Task::BlockInfos => {
                                    let index = BlockIndex::build(&src)?;
                                    let n = copy_block_infos(
                                        &src,
                                        &dst,
                                        &dst_path.join(".block_index_tmp"),
                                        &index,
                                    )?;
                                    // Same blocks as the destination's CF 3.
                                    index.save_to(&dst_path.join(block_index::FILE))?;
                                    records[2].fetch_add(n as u64, Ordering::Relaxed);
                                    done_tasks.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                                Task::Copy(col) => {
                                    let cf = src.cf_handle(col.to_str()).unwrap();
                                    let mut sink = SstSink::new(
                                        opts_of(col),
                                        tmp_dir.clone(),
                                        col.to_str().to_string(),
                                        ROLL_BYTES,
                                    );
                                    let mut n = 0u64;
                                    for item in src.iterator_cf_opt(
                                        cf,
                                        scan_options(None, None),
                                        IteratorMode::Start,
                                    ) {
                                        let (key, value) = item?;
                                        bytes_read.fetch_add(
                                            (key.len() + value.len()) as u64,
                                            Ordering::Relaxed,
                                        );
                                        sink.put(&key, &value)?;
                                        n += 1;
                                    }
                                    records[2].fetch_add(n, Ordering::Relaxed);
                                    (col, 0, sink.finish()?)
                                }
                                Task::Versioned { col, shard, lo, hi } => {
                                    let storage = col == StorageTypeColumn::AddressToStorage;
                                    let key_len = if storage { 64 } else { 32 };
                                    let cf = src.cf_handle(col.to_str()).unwrap();
                                    let mut sink = SstSink::new(
                                        opts_of(col),
                                        tmp_dir.clone(),
                                        format!("{}_{shard:03}", col.to_str()),
                                        ROLL_BYTES,
                                    );
                                    let mut prev = Vec::with_capacity(key_len);
                                    let (mut n, mut read) = (0u64, 0u64);
                                    for item in src.iterator_cf_opt(
                                        cf,
                                        scan_options(Some(lo), hi),
                                        IteratorMode::Start,
                                    ) {
                                        let (key, value) = item?;
                                        let user = &key[..key_len];
                                        let raw = u64::from_be_bytes(
                                            key[key.len() - 8..].try_into().unwrap(),
                                        );
                                        if user != prev.as_slice() {
                                            sink.roll_if_full()?;
                                            prev.clear();
                                            prev.extend_from_slice(user);
                                        }
                                        let value: &[u8] = if storage {
                                            strip_leading_zeros(&value)
                                        } else {
                                            &value
                                        };
                                        sink.put_with_ts(user, &encode_ts(u64::MAX - raw), value)?;
                                        n += 1;
                                        read += (key.len() + value.len()) as u64;
                                        if n % 100_000 == 0 {
                                            bytes_read.fetch_add(read, Ordering::Relaxed);
                                            read = 0;
                                            records[usize::from(!storage)]
                                                .fetch_add(100_000, Ordering::Relaxed);
                                        }
                                    }
                                    bytes_read.fetch_add(read, Ordering::Relaxed);
                                    records[usize::from(!storage)]
                                        .fetch_add(n % 100_000, Ordering::Relaxed);
                                    (col, shard, sink.finish()?)
                                }
                            };
                            outputs.lock().unwrap().push((col, shard, paths));
                            done_tasks.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            let mut result = Ok(());
            for w in workers {
                let r = w
                    .join()
                    .unwrap_or_else(|_| Err(Error::UnSupported("convert worker panicked".into())));
                if r.is_err() {
                    stop.store(true, Ordering::Relaxed);
                    if result.is_ok() {
                        result = r;
                    }
                }
            }
            stop.store(true, Ordering::Relaxed);
            result
        })?;

        let mut outputs = outputs.into_inner().unwrap();
        outputs.sort_by_key(|(col, shard, _)| (*col as u16, *shard));
        let mut report = UdtConvertReport {
            accounts: records[1].load(Ordering::Relaxed),
            storage: records[0].load(Ordering::Relaxed),
            other: records[2].load(Ordering::Relaxed),
            files: 0,
        };
        for &col in &cols {
            let paths: Vec<PathBuf> = outputs
                .iter()
                .filter(|(c, _, _)| *c == col)
                .flat_map(|(_, _, p)| p.iter().cloned())
                .collect();
            report.files += paths.len();
            if paths.is_empty() {
                continue;
            }
            let ingest_start = std::time::Instant::now();
            dst.ingest_external_file_cf_opts(
                dst.cf_handle(col.to_str()).unwrap(),
                &ingest_external_file_options(),
                paths,
            )?;
            info!(target: "migrate", "udt convert: ingested {col} in {}", fmt_hms(ingest_start.elapsed().as_secs_f64()));
            // Filter and index sizes show the files kept the CF's table options.
            if let Some(props) = dst.property_value_cf(
                dst.cf_handle(col.to_str()).unwrap(),
                "rocksdb.aggregated-table-properties",
            )? {
                info!(target: "migrate", "udt convert: {col} table properties: {props}");
            }
        }
        dst.flush()?;
        drop(dst);
        let _ = std::fs::remove_dir(&tmp_dir);
        write_schema_file(dst_path)?;
        info!(target: "migrate", "udt convert: done in {}: {report:?}", fmt_hms(start.elapsed().as_secs_f64()));
        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// Verification against the v2 source
// ---------------------------------------------------------------------------

/// Result of [`verify_udt_against_v2`].
#[derive(Debug, Default, Clone)]
pub struct UdtVerifyReport {
    pub keys: u64,
    pub reads: u64,
    pub absent_keys: u64,
    pub mismatches: u64,
}

/// Versions of one key kept for a check; hotter keys are checked over their
/// newest versions only.
const VERIFY_MAX_VERSIONS: usize = 4096;

/// Compare a v3 archive with the v2 archive it was converted from: for
/// random keys of each versioned CF, the value and block read at heights
/// around every version, and absent keys.
pub fn verify_udt_against_v2(
    v2_path: &Path,
    udt_path: &Path,
    samples: u64,
    seed: u64,
) -> Result<UdtVerifyReport, Error> {
    let v2 = open_read_only(v2_path, false)?;
    let v3 = open_read_only(udt_path, true)?;
    let mut rng = seed | 1;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut report = UdtVerifyReport::default();
    for col in [
        StorageTypeColumn::AddressToAccount,
        StorageTypeColumn::AddressToStorage,
    ] {
        let storage = col == StorageTypeColumn::AddressToStorage;
        let key_len = if storage { 64 } else { 32 };
        let cf = v2.cf_handle(col.to_str()).unwrap();
        // Random seeks: no scan readahead, which reads megabytes per level.
        let mut ro = ReadOptions::default();
        ro.set_total_order_seek(true);
        let mut iter = v2.raw_iterator_cf_opt(cf, ro);
        let same = |a: &[u8], b: &[u8]| {
            if storage {
                decode_storage(a) == decode_storage(b)
            } else {
                a == b
            }
        };
        for i in 0..samples {
            if i > 0 && i % 5000 == 0 {
                info!(target: "migrate", "udt verify: {col} {i}/{samples} keys, {report:?}");
            }
            let probe: Vec<u8> = (0..key_len).map(|_| next() as u8).collect();
            iter.seek(&probe);
            let Some(first) = iter.key() else {
                iter.status()?;
                continue;
            };
            let key = first[..key_len].to_vec();
            // Newest first in v2.
            let mut versions: Vec<(u64, Vec<u8>)> = Vec::new();
            let mut capped = false;
            while let Some(k) = iter.key() {
                if k[..key_len] != key[..] {
                    break;
                }
                if versions.len() == VERIFY_MAX_VERSIONS {
                    capped = true;
                    break;
                }
                let raw = u64::from_be_bytes(k[k.len() - 8..].try_into().unwrap());
                versions.push((u64::MAX - raw, iter.value().unwrap().to_vec()));
                iter.next();
            }
            iter.status()?;
            versions.reverse();
            report.keys += 1;
            let oldest = versions[0].0;
            let newest = versions.last().unwrap().0;
            let mut heights = vec![newest + 1 + next() % 1000];
            for _ in 0..8 {
                let (b, _) = &versions[(next() % versions.len() as u64) as usize];
                heights.push(*b);
                if *b > 0 && (!capped || *b > oldest) {
                    heights.push(b - 1);
                }
                let lo = if capped { oldest } else { 0 };
                heights.push(lo + next() % (newest - lo + 2));
            }
            for h in heights {
                let i = versions.partition_point(|(b, _)| *b <= h);
                let want = (i > 0).then(|| &versions[i - 1]);
                let got = UdtReader::new(h).get(&v3, col, &key)?;
                report.reads += 1;
                let ok = match (want, &got) {
                    (None, None) => true,
                    (Some((b, v)), Some(f)) => *b == f.block && same(v, f.value()),
                    _ => false,
                };
                if !ok {
                    report.mismatches += 1;
                    if report.mismatches <= 20 {
                        warn!(target: "migrate",
                            "udt verify mismatch: {col} key {} at {h}: v2 {:?}, v3 {:?}",
                            hex(&key), want.map(|(b, v)| (b, hex(v))),
                            got.as_ref().map(|f| (f.block, hex(f.value()))));
                    }
                }
            }
            // A key next to it that v2 does not hold.
            let mut absent = key.clone();
            absent[key_len - 1] ^= 0x5a;
            iter.seek(&absent);
            let held = iter.key().is_some_and(|k| k[..key_len] == absent[..]);
            if !held {
                report.absent_keys += 1;
                report.reads += 1;
                if UdtReader::new(u64::MAX - 1)
                    .get(&v3, col, &absent)?
                    .is_some()
                {
                    report.mismatches += 1;
                    warn!(target: "migrate", "udt verify: {col} key {} absent in v2 but found", hex(&absent));
                }
            }
        }
    }
    Ok(report)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use crate::db::{StateDBRead, StateDBWrapper};
    use crate::interface::EvmStorageWrite;
    use leafage_evm_types::{AccountStorageDiff, BlockStorageDiff, IndexValuePair};

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn block_hash(number: u64) -> H256 {
        let mut hash = H256::ZERO;
        hash.0[24..].copy_from_slice(&number.to_be_bytes());
        hash.0[0] = 0xb1;
        hash
    }

    fn block_info(number: u64) -> BlockInfo {
        let raw = RawHeader {
            number,
            ..Default::default()
        };
        let hash = block_hash(number);
        BlockInfo::new(Block {
            header: Header {
                hash,
                inner: raw,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn addrs() -> Vec<H256> {
        // Spread over the shards (first byte), some sharing one.
        (0..12u8)
            .map(|i| {
                let mut a = H256::repeat_byte(i.wrapping_mul(37));
                a.0[0] = [0x05, 0x45, 0x85, 0xc5, 0x05, 0x46][i as usize % 6];
                a.0[31] = i;
                a
            })
            .collect()
    }

    fn slots() -> Vec<H256> {
        (0..6u8).map(|i| H256::repeat_byte(0x10 + i)).collect()
    }

    fn random_diff(rng: &mut Rng, n: u64) -> BlockStorageDiff {
        let mut diff = BlockStorageDiff::default();
        for a in addrs() {
            match rng.below(10) {
                0 if n > 3 => diff.deleted_accounts.push(a),
                1..=4 => {
                    diff.new_accounts.push(NewAccount {
                        address: a,
                        balance: U256::from(rng.below(1_000_000)),
                        nonce: n,
                        code_hash: KECCAK256_EMPTY.0.into(),
                    });
                    let mut diffs = Vec::new();
                    for s in slots() {
                        if rng.below(3) != 0 {
                            continue;
                        }
                        // Zeros too, and values of every length.
                        let value = match rng.below(4) {
                            0 => U256::ZERO,
                            1 => U256::from(rng.below(256)),
                            2 => U256::MAX - U256::from(rng.below(1000)),
                            _ => U256::from(rng.below(u64::MAX)) << 64,
                        };
                        diffs.push(IndexValuePair { index: s, value });
                    }
                    diff.storage_diffs
                        .push(AccountStorageDiff { address: a, diffs });
                }
                _ => {}
            }
        }
        diff
    }

    type Answers = Vec<(
        Option<NewAccount>,
        Vec<(U256, Option<u64>)>,
        Vec<(H256, U256)>,
    )>;

    /// Every account, slot and slot listing at every height, read in a
    /// scrambled order (the state cache sees reads at mixed heights).
    fn read_all(db: &Arc<DataBaseRef>, head: u64, rng: &mut Rng) -> Vec<Answers> {
        let mut out: Vec<Answers> = vec![Vec::new(); head as usize + 1];
        let mut heights: Vec<u64> = (1..=head).collect();
        for i in (1..heights.len()).rev() {
            heights.swap(i, rng.below(i as u64 + 1) as usize);
        }
        for h in heights {
            let state = db
                .db_at(BlockId::Number(BlockNumberOrTag::Number(h)))
                .unwrap()
                .unwrap();
            // Batch first so cold misses exercise MultiGet. The existing v2
            // oracle also checks the spans learned by these reads on v3.
            let mut addresses = addrs();
            addresses.extend([addresses[0], H256::repeat_byte(0xff)]);
            let keys: Vec<_> = addresses
                .iter()
                .flat_map(|a| slots().into_iter().map(|s| (*a, s)))
                .collect();
            let accounts = state.read_account_many(&addresses).unwrap();
            let storage = state.read_storage_many(&keys).unwrap();
            assert!(state.read_account_many(&[]).unwrap().is_empty());
            assert!(state.read_storage_many(&[]).unwrap().is_empty());
            assert!(
                !state.supports_batched_reads(),
                "archive must not enable eager prefetch"
            );
            out[h as usize] = addresses
                .into_iter()
                .enumerate()
                .map(|(i, a)| {
                    let account = state.read_account(a).unwrap();
                    assert_eq!(accounts[i], account);
                    let slots: Vec<_> = slots()
                        .into_iter()
                        .map(|s| state.read_storage_and_height(a, s).unwrap())
                        .collect();
                    assert_eq!(
                        storage[i * slots.len()..(i + 1) * slots.len()],
                        slots.iter().map(|(value, _)| *value).collect::<Vec<_>>()
                    );
                    let listed = state.read_storage_slots(a).unwrap().unwrap();
                    (account, slots, listed)
                })
                .collect();
        }
        out
    }

    fn append(db: &Arc<DataBaseRef>, diffs: &[(u64, BlockStorageDiff)]) {
        for (n, diff) in diffs {
            let state = StateDBWrapper(
                db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                    .unwrap()
                    .unwrap(),
            );
            state.update_block(block_info(*n), diff.clone()).unwrap();
        }
    }

    #[test]
    fn test_udt_archive_matches_v2() {
        udt_archive_matches_v2(false);
    }

    #[test]
    fn test_udt_archive_matches_v2_block_index() {
        udt_archive_matches_v2(true);
    }

    /// Convert a v2 archive, with or without `ARCHIVE_BLOCK_INDEX=1`, and
    /// check the result reads as the source, before and after appending.
    fn udt_archive_matches_v2(by_number: bool) {
        let _g = ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::db_impl::archive_encoding::set_inverted_block_encoding(true);
        let base =
            std::env::temp_dir().join(format!("leafage-udt-{by_number}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (v2_dir, v3_dir) = (base.join("v2"), base.join("v3"));
        let mut rng = Rng(0x5eed);
        let (converted, head) = (40u64, 60u64);
        let diffs: Vec<(u64, BlockStorageDiff)> =
            (1..=head).map(|n| (n, random_diff(&mut rng, n))).collect();
        let split = converted as usize;

        // A v2 archive up to `converted`, marked inverted like production.
        {
            let db = Arc::new(DataBaseRef::open(&v2_dir, 64, false, false));
            db.write_encoding_marker(true).unwrap();
            append(&db, &diffs[..split]);
        }
        if by_number {
            std::env::set_var("ARCHIVE_BLOCK_INDEX", "1");
        }
        let report = DataBaseRef::convert_inverted_to_udt(&v2_dir, &v3_dir, 64, 3);
        std::env::remove_var("ARCHIVE_BLOCK_INDEX");
        let report = report.unwrap();
        assert!(report.accounts > 0 && report.storage > 0, "{report:?}");
        let verify = verify_udt_against_v2(&v2_dir, &v3_dir, 300, 7).unwrap();
        assert_eq!(verify.mismatches, 0, "{verify:?}");
        assert!(verify.keys > 0 && verify.absent_keys > 0, "{verify:?}");
        // Block infos move to `BlockNumToBlockInfo` only when asked for,
        // with the index saved.
        let cols = existing_columns(&v3_dir).unwrap();
        assert_eq!(
            cols.contains(&StorageTypeColumn::BlockNumToBlockInfo),
            by_number
        );
        assert_eq!(
            cols.contains(&StorageTypeColumn::BlockHashToBlockInfo),
            !by_number
        );
        assert_eq!(v3_dir.join(block_index::FILE).exists(), by_number);
        let infos = |db: &DataBaseRef| -> Vec<Option<BlockInfo>> {
            (1..=head + 1)
                .map(|n| db.read_block_info(block_hash(n)).unwrap())
                .collect()
        };

        // Serve v3, then append above the converted head through the v3
        // write path.
        let v3 = {
            let db = Arc::new(DataBaseRef::open(&v3_dir, 64, false, false));
            assert!(db.udt);
            assert!(db.state_cache.is_some());
            assert_eq!(db.block_index.is_some(), by_number);
            let converted_infos = infos(&db);
            let converted_reads = read_all(&db, converted, &mut rng);
            append(&db, &diffs[split..]);
            let appended_reads = read_all(&db, head, &mut rng);
            db.flush().unwrap();
            db.compact().unwrap();
            (
                converted_reads,
                appended_reads,
                read_all(&db, head, &mut rng),
                converted_infos,
                infos(&db),
            )
        };
        let v2 = {
            let db = Arc::new(DataBaseRef::open(&v2_dir, 64, false, false));
            assert!(!db.udt);
            let converted_infos = infos(&db);
            append(&db, &diffs[split..]);
            (read_all(&db, head, &mut rng), converted_infos, infos(&db))
        };
        assert!(v2.1[..split].iter().all(Option::is_some));
        assert_eq!(v2.1, v3.3, "converted block infos");
        assert_eq!(v2.2, v3.4, "appended block infos");
        let v2 = v2.0;
        assert_eq!(&v2[..=split], &v3.0[..], "converted");
        assert_eq!(v2, v3.1, "converted + appended");
        assert_eq!(v2, v3.2, "converted + appended, compacted");
        let _ = std::fs::remove_dir_all(&base);
        crate::db_impl::archive_encoding::set_inverted_block_encoding(false);
    }

    #[test]
    fn test_udt_next_version_bounds() {
        let _g = ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("leafage-udt-boundary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        init_archive_schema(&dir, ArchiveSchema::V3).unwrap();
        let db = DataBaseRef::open(&dir, 16, true, false);
        let columns = [
            (StorageTypeColumn::AddressToAccount, 32),
            (StorageTypeColumn::AddressToStorage, 64),
        ];
        let mut batch = WriteBatch::default();
        for (col, len) in columns {
            let cf = db.db.cf_handle(col.to_str()).unwrap();
            for byte in [0, 0xff] {
                for version in [10, 100, 200, u64::MAX] {
                    // Empty values represent archive deletion/zero and still
                    // end the preceding version's interval.
                    let value: &[u8] = if version == 100 { &[] } else { &[1] };
                    batch.put_cf_with_ts(cf, vec![byte; len], encode_ts(version), value);
                }
            }
        }
        db.db.write(batch).unwrap();
        for phase in 0..3 {
            if phase == 1 {
                db.flush().unwrap();
            }
            if phase == 2 {
                db.compact().unwrap();
            }
            for (col, len) in columns {
                assert_eq!(
                    next_version(db.db, col, &vec![0x80; len], 0, u64::MAX).unwrap(),
                    None
                );
                for byte in [0, 0xff] {
                    for (block, head, expected) in [
                        (0, 9, None),
                        (0, 10, Some(10)),
                        (9, 200, Some(10)),
                        (10, 199, Some(100)),
                        (99, 100, Some(100)),
                        (100, 199, None),
                        (100, 200, Some(200)),
                        (200, 300, None),
                        (u64::MAX - 1, u64::MAX, Some(u64::MAX)),
                    ] {
                        assert_eq!(
                            next_version(db.db, col, &vec![byte; len], block, head).unwrap(),
                            expected,
                            "phase {phase}, {col:?}, key {byte}, ({block}, {head}]"
                        );
                    }
                }
            }
        }
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Repeated misses learn the whole version interval, including absence.
    #[test]
    fn test_udt_state_cache_probe() {
        let _g = ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::db_impl::archive_encoding::set_inverted_block_encoding(true);
        let base = std::env::temp_dir().join(format!("leafage-udt-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (v2_dir, v3_dir) = (base.join("v2"), base.join("v3"));
        let (stable, changing) = (H256::repeat_byte(0x21), H256::repeat_byte(0x22));
        let slot = H256::repeat_byte(0x10);
        let write = |address: H256, value: u64| AccountStorageDiff {
            address,
            diffs: vec![IndexValuePair {
                index: slot,
                value: U256::from(value),
            }],
        };
        let diffs: Vec<(u64, BlockStorageDiff)> = (1..=120u64)
            .map(|n| {
                let mut diff = BlockStorageDiff::default();
                match n {
                    10 => diff.storage_diffs = vec![write(stable, 1), write(changing, 2)],
                    100 => diff.storage_diffs = vec![write(changing, 3)],
                    110 => {
                        diff.storage_diffs = vec![write(changing, 0)];
                        diff.deleted_accounts.push(changing);
                    }
                    115 => diff.storage_diffs = vec![write(changing, 4)],
                    _ => {}
                }
                diff.new_accounts = diff
                    .storage_diffs
                    .iter()
                    .filter(|_| n != 110)
                    .map(|d| NewAccount {
                        address: d.address,
                        balance: d.diffs[0].value,
                        nonce: n,
                        code_hash: KECCAK256_EMPTY,
                    })
                    .collect();
                (n, diff)
            })
            .collect();
        {
            let db = Arc::new(DataBaseRef::open(&v2_dir, 64, false, false));
            db.write_encoding_marker(true).unwrap();
            append(&db, &diffs);
        }
        DataBaseRef::convert_inverted_to_udt(&v2_dir, &v3_dir, 64, 3).unwrap();
        {
            let db = Arc::new(DataBaseRef::open(&v3_dir, 64, false, false));
            assert!(db.udt);
            let read = |n: u64, address: H256| {
                db.db_at(BlockId::Number(BlockNumberOrTag::Number(n)))
                    .unwrap()
                    .unwrap()
                    .read_storage_and_height(address, slot)
                    .unwrap()
            };
            let key = |address: H256| {
                let mut k = [0u8; 64];
                k[..32].copy_from_slice(address.as_slice());
                k[32..].copy_from_slice(slot.as_slice());
                k
            };
            let never = H256::repeat_byte(0x23);
            // The first read caches [10, 15]; the second, a miss at 16 on a
            // key read before, probes.
            for n in [15, 16] {
                assert_eq!(read(n, stable), (U256::from(1), Some(10)));
                assert_eq!(read(n, changing), (U256::from(2), Some(10)));
                assert_eq!(read(n, never), (U256::ZERO, None));
                let state = db
                    .db_at(BlockId::Number(BlockNumberOrTag::Number(n)))
                    .unwrap()
                    .unwrap();
                // Nine items also exercise the actual MultiGet path.
                let accounts = state
                    .read_account_many(&[stable, changing, never].repeat(3))
                    .unwrap();
                assert_eq!(
                    accounts
                        .iter()
                        .map(|a| a.as_ref().map(|a| a.balance))
                        .collect::<Vec<_>>(),
                    [Some(U256::from(1)), Some(U256::from(2)), None].repeat(3)
                );
            }
            let cache = db.state_cache.as_ref().unwrap();
            // A failed optional probe must not fail an already successful
            // read or cache any unproven heights. A non-timestamped CF makes
            // RocksDB return InvalidArgument without fault-injection hooks.
            let failed_key = key(H256::repeat_byte(0x24));
            let mut span = Span {
                lo: 10,
                hi: 15,
                found: true,
                value: U256::from(1),
            };
            cache.storage.record_udt(failed_key, span, None);
            let Lookup::Miss(prev) = cache.storage.lookup(&failed_key, 16) else {
                panic!("expected miss")
            };
            let state = db
                .db_at(BlockId::Number(BlockNumberOrTag::Number(16)))
                .unwrap()
                .unwrap();
            span.hi = 16;
            assert!(state
                .udt_record(
                    state.cache_view.as_ref().unwrap(),
                    &cache.storage,
                    &prev,
                    StorageTypeColumn::HashToCode,
                    failed_key,
                    span
                )
                .is_ok());
            assert!(matches!(
                cache.storage.lookup(&failed_key, 16),
                Lookup::Hit(Span { hi: 16, .. })
            ));
            assert!(matches!(
                cache.storage.lookup(&failed_key, 17),
                Lookup::Miss(_)
            ));
            assert!(matches!(
                cache.storage.lookup(&key(stable), 120),
                Lookup::Hit(_)
            ));
            assert!(matches!(
                cache.storage.lookup(&key(never), 120),
                Lookup::Hit(Span { found: false, .. })
            ));
            assert!(matches!(
                cache.storage.lookup(&key(changing), 99),
                Lookup::Hit(Span { lo: 10, hi: 99, .. })
            ));
            assert!(matches!(
                cache.storage.lookup(&key(changing), 100),
                Lookup::Miss(_)
            ));
            assert!(matches!(
                cache.accounts.lookup(&changing.0, 99),
                Lookup::Hit(Span { lo: 10, hi: 99, .. })
            ));
            assert!(matches!(
                cache.accounts.lookup(&changing.0, 100),
                Lookup::Miss(_)
            ));
            // The exact boundary includes 99 but never version 100.
            assert_eq!(read(50, changing), (U256::from(2), Some(10)));
            assert!(matches!(
                cache.storage.lookup(&key(changing), 50),
                Lookup::Hit(Span { lo: 10, hi: 99, .. })
            ));
            assert_eq!(read(99, changing), (U256::from(2), Some(10)));
            assert_eq!(read(100, changing), (U256::from(3), Some(100)));
            assert_eq!(read(109, changing), (U256::from(3), Some(100)));
            assert_eq!(read(110, changing), (U256::ZERO, Some(110)));
            assert_eq!(read(115, changing), (U256::from(4), Some(115)));
            let state = db
                .db_at(BlockId::Number(BlockNumberOrTag::Number(110)))
                .unwrap()
                .unwrap();
            assert_eq!(state.read_account(changing).unwrap(), None);
            assert_eq!(read(120, stable), (U256::from(1), Some(10)));
        }
        let _ = std::fs::remove_dir_all(&base);
        crate::db_impl::archive_encoding::set_inverted_block_encoding(false);
    }

    /// A new archive created as v3 answers as one created as v2; an existing
    /// archive keeps its schema.
    #[test]
    fn test_new_udt_archive_matches_v2() {
        let _g = ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("leafage-udt-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut rng = Rng(0xab1e);
        let head = 40u64;
        let mut diffs: Vec<(u64, BlockStorageDiff)> =
            (1..=head).map(|n| (n, random_diff(&mut rng, n))).collect();
        let codes: Vec<_> = (0..12u8)
            .map(|i| leafage_evm_types::NewCode {
                code_hash: H256::repeat_byte(0x80 + i),
                code: Bytes::from(vec![i; 40]),
            })
            .collect();
        diffs[0].1.new_codes = codes.clone();
        let mut answers = Vec::new();
        for schema in [ArchiveSchema::V2, ArchiveSchema::V3] {
            let dir = base.join(schema.to_string());
            assert!(init_archive_schema(&dir, schema).unwrap());
            crate::db_impl::archive_encoding::set_inverted_block_encoding(schema.inverted());
            {
                let db = Arc::new(DataBaseRef::open(&dir, 64, false, false));
                assert_eq!(db.udt, schema == ArchiveSchema::V3);
                db.write_encoding_marker(schema.inverted()).unwrap();
                append(&db, &diffs);
            }
            // Reopened, whatever schema is asked for.
            assert!(!init_archive_schema(&dir, ArchiveSchema::V1).unwrap());
            assert_eq!(is_udt_archive(&dir).unwrap(), schema == ArchiveSchema::V3);
            let mut db = Arc::new(DataBaseRef::open(&dir, 64, false, false));
            let mut hashes: Vec<_> = codes.iter().rev().map(|code| code.code_hash).collect();
            hashes.extend([hashes[0], H256::repeat_byte(0xff)]);
            let expected: Vec<_> = hashes
                .iter()
                .map(|hash| {
                    codes
                        .iter()
                        .find(|code| code.code_hash == *hash)
                        .map(|code| code.code.clone())
                })
                .collect();
            db.read_code(hashes[0]).unwrap(); // Mixed cache hits and batch misses.
            for _ in 0..2 {
                assert_eq!(db.read_code_many(&hashes).unwrap(), expected);
            }
            assert!(db.read_code_many(&[]).unwrap().is_empty());
            answers.push(read_all(&db, head, &mut rng));
            let uncached = Arc::get_mut(&mut db).unwrap();
            uncached.state_cache = None;
            uncached.code_cache = None;
            assert_eq!(db.read_code_many(&hashes).unwrap(), expected);
            assert_eq!(answers.last().unwrap(), &read_all(&db, head, &mut rng));
        }
        assert_eq!(answers[0], answers[1]);

        // A schema file left before RocksDB created the database is dropped
        // when a new v1 / v2 archive is asked for.
        let dir = base.join("stale");
        assert!(init_archive_schema(&dir, ArchiveSchema::V3).unwrap());
        assert!(is_udt_archive(&dir).unwrap());
        assert!(init_archive_schema(&dir, ArchiveSchema::V1).unwrap());
        assert!(!is_udt_archive(&dir).unwrap());
        let _ = std::fs::remove_dir_all(&base);
        crate::db_impl::archive_encoding::set_inverted_block_encoding(false);
    }

    /// `archive-init`'s bulk load of pre-encoded versions: into a new v2
    /// archive as SST files ingested batch by batch, into a new v3 archive as
    /// write batches committed in block order. Both answer alike.
    #[test]
    fn test_udt_bulk_load_matches_v2() {
        use crate::db_impl::archive_encoding::{
            encode_account_key, encode_slim_account, encode_storage_key,
        };
        let _g = ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::db_impl::archive_encoding::set_inverted_block_encoding(true);
        let base = std::env::temp_dir().join(format!("leafage-udt-sst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut rng = Rng(0x55d7);
        let head = 40u64;
        let diffs: Vec<(u64, BlockStorageDiff)> =
            (1..=head).map(|n| (n, random_diff(&mut rng, n))).collect();
        let mut answers = Vec::new();
        for schema in [ArchiveSchema::V2, ArchiveSchema::V3] {
            let dir = base.join(schema.to_string());
            assert!(init_archive_schema(&dir, schema).unwrap());
            let db = Arc::new(DataBaseRef::open(&dir, 64, false, false));
            db.write_encoding_marker(true).unwrap();
            for (i, batch) in diffs.chunks(10).enumerate() {
                let (mut accounts, mut storage) = (Vec::new(), Vec::new());
                for (n, diff) in batch {
                    // Block metadata through the regular path, state as SSTs.
                    append(&db, &[(*n, BlockStorageDiff::default())]);
                    for a in &diff.deleted_accounts {
                        accounts.push((encode_account_key(*a, *n), Vec::new()));
                    }
                    for a in &diff.new_accounts {
                        accounts.push((
                            encode_account_key(a.address, *n),
                            encode_slim_account(a.clone()),
                        ));
                    }
                    for d in &diff.storage_diffs {
                        for p in &d.diffs {
                            storage.push((
                                encode_storage_key(d.address, p.index, *n),
                                p.value.to_be_bytes::<32>(),
                            ));
                        }
                    }
                }
                accounts.sort_by_key(|a| a.0);
                accounts.dedup_by(|later, earlier| {
                    let same = later.0 == earlier.0;
                    if same {
                        std::mem::swap(later, earlier);
                    }
                    same
                });
                storage.sort_by_key(|a| a.0);
                storage.dedup_by(|later, earlier| {
                    let same = later.0 == earlier.0;
                    if same {
                        std::mem::swap(later, earlier);
                    }
                    same
                });
                if db.is_schema_v3() {
                    let mut batch = db.prepare_write_batch().unwrap();
                    batch.extend_account_writes(accounts.into_iter().map(|(k, v)| (k, Some(v))));
                    batch.extend_storage_writes(storage);
                    db.commit(batch).unwrap();
                } else {
                    let acc = dir.join(format!("acc_{i}.sst"));
                    let sto = dir.join(format!("sto_{i}.sst"));
                    db.write_account_sst(&acc, &accounts).unwrap();
                    db.write_storage_sst(&sto, &storage).unwrap();
                    db.ingest_account_ssts(vec![acc]).unwrap();
                    db.ingest_storage_ssts(vec![sto]).unwrap();
                }
            }
            answers.push(read_all(&db, head, &mut rng));
        }
        assert_eq!(answers[0], answers[1]);
        let _ = std::fs::remove_dir_all(&base);
        crate::db_impl::archive_encoding::set_inverted_block_encoding(false);
    }

    /// Rewind below the converted head (trimming ingested and flushed files
    /// alike), then replay a different chain: v3 must answer as v2.
    #[test]
    fn test_udt_rewind_matches_v2() {
        use crate::{MultiStorage, StorageKind};
        let _g = ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::db_impl::archive_encoding::set_inverted_block_encoding(true);
        let base = std::env::temp_dir().join(format!("leafage-udt-rewind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (v2_dir, v3_dir) = (base.join("v2"), base.join("v3"));
        let mut rng = Rng(0xfeed);
        let (converted, old_head, target, new_head) = (30u64, 50u64, 20u64, 60u64);
        let old: Vec<(u64, BlockStorageDiff)> = (1..=old_head)
            .map(|n| (n, random_diff(&mut rng, n)))
            .collect();
        let new: Vec<(u64, BlockStorageDiff)> = (target + 1..=new_head)
            .map(|n| (n, random_diff(&mut rng, n)))
            .collect();
        {
            let db = Arc::new(DataBaseRef::open(&v2_dir, 64, false, false));
            db.write_encoding_marker(true).unwrap();
            append(&db, &old[..converted as usize]);
        }
        DataBaseRef::convert_inverted_to_udt(&v2_dir, &v3_dir, 64, 2).unwrap();
        for dir in [&v2_dir, &v3_dir] {
            let db = Arc::new(DataBaseRef::open(dir, 64, false, false));
            // Two flushed files above the ingested ones, and a memtable.
            append(&db, &old[converted as usize..40]);
            db.flush().unwrap();
            append(&db, &old[40..]);
        }
        let mut answers = Vec::new();
        for dir in [&v2_dir, &v3_dir] {
            let rewound = MultiStorage::rewind_archive_offline(
                dir,
                64,
                StorageKind::Rocksdb,
                target,
                None,
                &base.join("offset"),
            )
            .unwrap();
            assert_eq!(rewound.header.number, target);
            let db = Arc::new(DataBaseRef::open(dir, 64, false, false));
            assert!(db
                .db_at(BlockId::Number(BlockNumberOrTag::Number(target + 1)))
                .unwrap()
                .is_none());
            if db.udt {
                // No version above the target is left anywhere.
                let all = UdtReader::new(u64::MAX);
                for a in addrs() {
                    let account = all.get(db.db, StorageTypeColumn::AddressToAccount, a.as_slice());
                    assert!(account.unwrap().is_none_or(|f| f.block <= target));
                    for s in slots() {
                        let mut key = [0u8; 64];
                        key[..32].copy_from_slice(a.as_slice());
                        key[32..].copy_from_slice(s.as_slice());
                        let slot = all.get(db.db, StorageTypeColumn::AddressToStorage, &key);
                        assert!(slot.unwrap().is_none_or(|f| f.block <= target));
                    }
                }
            }
            let rewound = read_all(&db, target, &mut rng);
            append(&db, &new);
            answers.push((rewound, read_all(&db, new_head, &mut rng)));
        }
        assert_eq!(answers[0].0, answers[1].0, "rewound");
        assert_eq!(answers[0].1, answers[1].1, "rewound + replayed");
        let _ = std::fs::remove_dir_all(&base);
        crate::db_impl::archive_encoding::set_inverted_block_encoding(false);
    }

    #[test]
    fn test_udt_comparator_loads_by_name() {
        let opts = base_cf_options();
        let dir = std::env::temp_dir().join(format!("leafage-udt-cmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        let db =
            DB::open_cf_descriptors(&db_opts, &dir, vec![ColumnFamilyDescriptor::new("v", opts)])
                .unwrap();
        let cf = db.cf_handle("v").unwrap();
        for (b, v) in [(5u64, b"a".as_slice()), (10, b""), (20, b"c")] {
            db.put_cf_with_ts(cf, b"k", encode_ts(b), v).unwrap();
        }
        let col = |db: &DB, h: u64| {
            // Same lookup as UdtReader, on a CF named "v".
            let r = UdtReader::new(h);
            let cf = db.cf_handle("v").unwrap();
            let (mut len, mut ts, mut ts_len) = (0usize, std::ptr::null_mut(), 0usize);
            let mut err: *mut c_char = std::ptr::null_mut();
            let value = unsafe {
                ffi::rocksdb_get_cf_with_ts(
                    db.as_raw_ptr(),
                    r.ro,
                    cf.inner(),
                    b"k".as_ptr() as *const c_char,
                    1,
                    &mut len,
                    &mut ts,
                    &mut ts_len,
                    &mut err,
                )
            };
            assert!(err.is_null());
            if ts_len == 0 {
                return None;
            }
            let block = unsafe {
                u64::from_le_bytes(
                    std::slice::from_raw_parts(ts as *const u8, 8)
                        .try_into()
                        .unwrap(),
                )
            };
            let f = Found { value, len, block };
            unsafe { ffi::rocksdb_free(ts as *mut c_void) };
            Some((f.block, f.value().to_vec()))
        };
        assert_eq!(col(&db, 4), None);
        assert_eq!(col(&db, 5), Some((5, b"a".to_vec())));
        assert_eq!(col(&db, 9), Some((5, b"a".to_vec())));
        assert_eq!(col(&db, 10), Some((10, Vec::new())));
        assert_eq!(col(&db, 25), Some((20, b"c".to_vec())));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
