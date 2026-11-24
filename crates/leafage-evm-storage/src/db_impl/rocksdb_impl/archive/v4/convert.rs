//! Offline conversion of a RocksDB archive (schema v1, v2 or v3) to schema
//! v4, and the check of the result against its source
//! (`docs/ArchiveSchemaV4.md`, "Conversion").

use super::super::block_index;
use super::super::udt::{
    self, encode_ts, hex, open_read_only, scan_options, write_schema_file, ArchiveSchema,
    SCHEMA_FILE,
};
use super::super::{
    decode_archive_header, decode_cached_account, encode_block_num, fmt_hms, shard_bounds,
    DataBaseRef, StorageTypeColumn, ENCODING_MARKER_KEY,
};
use super::codec::{key_len, Val};
use super::file::{self, Dict, PageWriter, Rec};
use super::side::{self, BlockRec, Blocks, BlocksMeta, Codes, CodesMeta};
use super::store::{NewBase, Options, Store};
use super::STORE_DIR;
use crate::db_impl::error::Error;
use rocksdb::{DBRawIteratorWithThreadMode, IteratorMode, ReadOptions, DB};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// Dictionary files of the converted store, per kind, before the store
/// names them.
const DICT_NAMES: [&str; 2] = ["convert-account.dict", "convert-storage.dict"];
/// Places in the key space sampled to train a kind's dictionary.
const DICT_SAMPLES: u64 = 2000;
/// Versions of one key held in memory by the conversion. A v2 or v3 source
/// sorts a key's versions newest first and v4 wants them oldest first; a key
/// with more versions than this (an account that changes in every block has
/// one per block of the chain) is read again in block windows of this many.
/// A v1 source sorts them oldest first, and they are passed on in windows
/// as they are read.
#[cfg(not(test))]
const KEY_WINDOW: usize = 1 << 18;
#[cfg(test)]
const KEY_WINDOW: usize = 12;
/// Keys the conversion passed whole, and windows of the others.
#[cfg(test)]
static KEY_PASSES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Records and bytes written by [`DataBaseRef::convert_to_v4`].
#[derive(Debug, Default, Clone)]
pub struct V4ConvertReport {
    /// Account versions.
    pub accounts: u64,
    /// Storage versions.
    pub storage: u64,
    /// Versions above the source's head, left behind.
    pub above_head: u64,
    /// Pointers to the latest version of a key that early v1 archives wrote
    /// beside each version (block `u64::MAX`), left behind.
    pub sentinels: u64,
    /// Records of the other column families.
    pub other: u64,
    /// Base files written, and their bytes.
    pub files: usize,
    pub bytes: u64,
    /// The last block of the archive.
    pub head: u64,
}

enum Task {
    /// The block infos, by number.
    Blocks,
    /// The code.
    Codes,
    /// Convert one key-range shard of a versioned CF.
    Versioned {
        storage: bool,
        lo: u8,
        hi: Option<u8>,
    },
}

/// Read options that yield every version of a timestamped CF: newest first
/// within a key, the key followed by its timestamp.
fn all_versions(mut ro: ReadOptions) -> ReadOptions {
    ro.set_timestamp(encode_ts(u64::MAX));
    ro.set_iter_start_ts(encode_ts(0));
    ro
}

/// The key and block of the record an iterator over a versioned CF of a
/// `schema` archive is on. No block: the record is not a version but the
/// pointer to the latest one that early v1 archives wrote beside each
/// (block `u64::MAX`), stale since.
fn version_key(schema: ArchiveSchema, raw: &[u8], k: usize) -> Result<(&[u8], Option<u64>), Error> {
    if schema == ArchiveSchema::V3 {
        // The key, its 8-byte timestamp, then RocksDB's sequence trailer.
        if raw.len() < k + 8 {
            return Err(Error::UnSupported(format!(
                "unexpected schema v3 key of {} bytes",
                raw.len()
            )));
        }
        return Ok((
            &raw[..k],
            Some(u64::from_le_bytes(raw[k..k + 8].try_into().unwrap())),
        ));
    }
    // The key, then a 32-byte tail that ends in the block (v1) or in
    // `u64::MAX` less the block (v2).
    if raw.len() != k + 32 {
        return Err(Error::UnSupported(format!(
            "unexpected schema {schema} key of {} bytes",
            raw.len()
        )));
    }
    let tail = u64::from_be_bytes(raw[k + 24..].try_into().unwrap());
    let block = if schema == ArchiveSchema::V1 {
        (tail != u64::MAX).then_some(tail)
    } else {
        Some(u64::MAX - tail)
    };
    Ok((&raw[..k], block))
}

/// Places of each versioned CF whose keys tell a v1 archive from a v2 one,
/// and records read at each.
const TAIL_SAMPLES: u64 = 64;
const TAIL_SAMPLE_RUN: usize = 32;

/// Whether the RocksDB archive `db`, one without a schema file, is v1 or
/// v2. Its keys tell: a block number is far below 2^63, so the tail of a
/// version is below it on v1 and above it on v2. The encoding marker, which
/// an archive from before the marker does not have, must agree with them.
fn tail_schema(db: &DB) -> Result<ArchiveSchema, Error> {
    let marker = match db.cf_handle(StorageTypeColumn::LatestBlockHash.to_str()) {
        Some(cf) => db
            .get_cf(cf, ENCODING_MARKER_KEY)?
            .and_then(|v| v.first().copied())
            .map(|b| b == 1),
        None => None,
    };
    // A tail of `u64::MAX` is either: v1's pointer to the latest version,
    // or block 0 on v2.
    let (mut ascending, mut inverted, mut ends) = (0u64, 0u64, 0u64);
    for storage in [false, true] {
        let k = key_len(storage);
        let col = if storage {
            StorageTypeColumn::AddressToStorage
        } else {
            StorageTypeColumn::AddressToAccount
        };
        let Some(cf) = db.cf_handle(col.to_str()) else {
            continue;
        };
        let mut ro = ReadOptions::default();
        ro.set_total_order_seek(true);
        ro.fill_cache(false);
        let mut iter = db.raw_iterator_cf_opt(cf, ro);
        for i in 0..TAIL_SAMPLES {
            iter.seek((i * (u64::MAX / TAIL_SAMPLES)).to_be_bytes());
            for _ in 0..TAIL_SAMPLE_RUN {
                let Some(raw) = iter.key() else {
                    break;
                };
                if raw.len() != k + 32 {
                    return Err(Error::UnSupported(format!(
                        "unexpected key of {} bytes in {col} of the source",
                        raw.len()
                    )));
                }
                match u64::from_be_bytes(raw[k + 24..].try_into().unwrap()) {
                    u64::MAX => ends += 1,
                    tail if tail >> 63 == 0 => ascending += 1,
                    _ => inverted += 1,
                }
                iter.next();
            }
            iter.status()?;
        }
    }
    let inverted = match (ascending > 0, inverted > 0, marker) {
        (true, true, _) => {
            return Err(Error::UnSupported(format!(
                "the source's keys are in both block encodings \
                 ({ascending} ascending, {inverted} inverted of those sampled)"
            )))
        }
        (true, false, Some(true)) | (false, true, Some(false)) => {
            return Err(Error::UnSupported(format!(
                "the source's encoding marker says inverted: {}, its keys say otherwise \
                 ({ascending} ascending, {inverted} inverted of those sampled)",
                marker == Some(true)
            )))
        }
        (true, false, _) => false,
        (false, true, _) => true,
        (false, false, Some(marker)) => marker,
        // Versions of block 0 alone are an inverted archive's; one without
        // versions converts the same either way.
        (false, false, None) => ends > 0,
    };
    Ok(if inverted {
        ArchiveSchema::V2
    } else {
        ArchiveSchema::V1
    })
}

/// Open the RocksDB archive at `path` read-only: the database, and the
/// schema its versioned column families are in.
fn open_source(path: &Path) -> Result<(DB, ArchiveSchema), Error> {
    match udt::schema_file(path)? {
        Some(3) => Ok((open_read_only(path, true)?, ArchiveSchema::V3)),
        Some(_) => Err(Error::UnSupported(format!(
            "{} is a schema v4 archive already",
            path.display()
        ))),
        None => {
            let db = open_read_only(path, false)?;
            let schema = tail_schema(&db)?;
            Ok((db, schema))
        }
    }
}

/// A value of a versioned CF (the same in v1, v2 and v3, but for the leading
/// zeros of a slot's) as a v4 one.
fn version_value(storage: bool, value: &[u8]) -> Result<Val, Error> {
    if storage {
        if value.len() > 32 {
            return Err(Error::UnSupported("slot value over 32 bytes".into()));
        }
        let mut v = [0u8; 32];
        v[32 - value.len()..].copy_from_slice(value);
        Ok(Val::Storage(v))
    } else {
        Ok(match decode_cached_account(value) {
            Some(account) => Val::Account {
                nonce: account.nonce,
                balance: account.balance.to_be_bytes(),
                code_hash: account.code_hash.0,
            },
            None => Val::Deleted,
        })
    }
}

/// Reads a versioned CF of a RocksDB archive key by key: each key's
/// versions, oldest first.
struct KeyVersions<'a> {
    db: &'a DB,
    schema: ArchiveSchema,
    iter: DBRawIteratorWithThreadMode<'a, DB>,
    storage: bool,
    /// Versions above this block are skipped and counted.
    head: u64,
    above_head: u64,
    /// Pointers to the latest version (v1), skipped and counted.
    sentinels: u64,
    /// The current key's versions; their `key` is the key.
    versions: Vec<Rec>,
    /// Of a key read in windows: one window, and the newest block of each
    /// window after the first, newest first.
    window: Vec<Rec>,
    cuts: Vec<u64>,
    bytes: u64,
}

impl<'a> KeyVersions<'a> {
    fn new(db: &'a DB, schema: ArchiveSchema, storage: bool, ro: ReadOptions, head: u64) -> Self {
        let col = if storage {
            StorageTypeColumn::AddressToStorage
        } else {
            StorageTypeColumn::AddressToAccount
        };
        let cf = db.cf_handle(col.to_str()).unwrap();
        let ro = if schema == ArchiveSchema::V3 {
            all_versions(ro)
        } else {
            ro
        };
        Self {
            db,
            schema,
            iter: db.raw_iterator_cf_opt(cf, ro),
            storage,
            head,
            above_head: 0,
            sentinels: 0,
            versions: Vec::new(),
            window: Vec::new(),
            cuts: Vec::new(),
            bytes: 0,
        }
    }

    fn cf(&self) -> &'a rocksdb::ColumnFamily {
        let col = if self.storage {
            StorageTypeColumn::AddressToStorage
        } else {
            StorageTypeColumn::AddressToAccount
        };
        self.db.cf_handle(col.to_str()).unwrap()
    }

    /// An iterator on the version of `key` at block `hi`, of a source that
    /// sorts versions newest first (v2, v3). On v3 it ends below block
    /// `lo`.
    fn window_iter(&self, key: &[u8], lo: u64, hi: u64) -> DBRawIteratorWithThreadMode<'a, DB> {
        let mut ro = ReadOptions::default();
        ro.set_verify_checksums(false);
        ro.set_total_order_seek(true);
        ro.set_readahead_size(2 << 20);
        ro.fill_cache(false);
        if self.schema == ArchiveSchema::V3 {
            ro.set_timestamp(encode_ts(hi));
            ro.set_iter_start_ts(encode_ts(lo));
            let mut iter = self.db.raw_iterator_cf_opt(self.cf(), ro);
            iter.seek(key);
            iter
        } else {
            let mut iter = self.db.raw_iterator_cf_opt(self.cf(), ro);
            let mut from = key.to_vec();
            from.extend_from_slice(&encode_block_num(u64::MAX - hi));
            iter.seek(&from);
            iter
        }
    }

    /// Pass every version up to the head of the key the iterator is on to
    /// `sink`, oldest first, and move past the key. `sink` is called once,
    /// or for a key with more than [`KEY_WINDOW`] versions once per window
    /// of them. `false` at the end.
    fn drain_key(
        &mut self,
        mut sink: impl FnMut(&[Rec]) -> Result<(), Error>,
    ) -> Result<bool, Error> {
        self.versions.clear();
        self.cuts.clear();
        let k = key_len(self.storage);
        let oldest_first = self.schema == ArchiveSchema::V1;
        let mut key = [0u8; 64];
        let mut have = false;
        // Versions up to the head, and those of them passed on.
        let mut n = 0usize;
        let mut passed = 0usize;
        while let Some(raw) = self.iter.key() {
            let (user, block) = version_key(self.schema, raw, k)?;
            if have && user != &key[..k] {
                break;
            }
            key[..k].copy_from_slice(user);
            have = true;
            let value = self.iter.value().unwrap_or(&[]);
            self.bytes += (raw.len() + value.len()) as u64;
            match block {
                None => self.sentinels += 1,
                Some(block) if block > self.head => self.above_head += 1,
                Some(block) => {
                    if oldest_first {
                        // In the order wanted: a full window goes on at once.
                        if self.versions.len() == KEY_WINDOW {
                            passed += self.versions.len();
                            sink(&self.versions)?;
                            self.versions.clear();
                            #[cfg(test)]
                            KEY_PASSES[1].fetch_add(1, Ordering::Relaxed);
                        }
                        self.versions.push(Rec {
                            key,
                            block,
                            val: version_value(self.storage, value)?,
                        });
                    } else if n < KEY_WINDOW {
                        self.versions.push(Rec {
                            key,
                            block,
                            val: version_value(self.storage, value)?,
                        });
                    } else if n.is_multiple_of(KEY_WINDOW) {
                        self.cuts.push(block);
                    }
                    n += 1;
                }
            }
            self.iter.next();
        }
        self.iter.status()?;
        if !have {
            return Ok(false);
        }
        passed += self.versions.len();
        // Of a source that sorts newest first, the windows below the newest
        // one, oldest first: each is read again from its newest block down.
        for i in (0..self.cuts.len()).rev() {
            let lo = match self.cuts.get(i + 1) {
                Some(&older) => older + 1,
                None => 0,
            };
            let mut iter = self.window_iter(&key[..k], lo, self.cuts[i]);
            self.window.clear();
            while let Some(raw) = iter.key() {
                let (user, block) = version_key(self.schema, raw, k)?;
                let Some(block) = block.filter(|&block| user == &key[..k] && block >= lo) else {
                    break;
                };
                self.window.push(Rec {
                    key,
                    block,
                    val: version_value(self.storage, iter.value().unwrap_or(&[]))?,
                });
                iter.next();
            }
            iter.status()?;
            passed += self.window.len();
            self.window.reverse();
            sink(&self.window)?;
            #[cfg(test)]
            KEY_PASSES[1].fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(test)]
        KEY_PASSES[usize::from(n > KEY_WINDOW)].fetch_add(1, Ordering::Relaxed);
        if passed != n {
            return Err(Error::UnSupported(format!(
                "key {} has {n} versions but {passed} in block windows",
                hex(&key[..k])
            )));
        }
        if !oldest_first {
            self.versions.reverse();
        }
        sink(&self.versions)?;
        Ok(true)
    }

    /// Load the versions of the key the iterator is on, at most `cap` of
    /// them (the newest), oldest first, and move past the key. `false` at
    /// the end.
    fn next_key(&mut self, cap: usize) -> Result<bool, Error> {
        self.versions.clear();
        let k = key_len(self.storage);
        let oldest_first = self.schema == ArchiveSchema::V1;
        let mut key = [0u8; 64];
        let mut have = false;
        while let Some(raw) = self.iter.key() {
            let (user, block) = version_key(self.schema, raw, k)?;
            if have && user != &key[..k] {
                break;
            }
            key[..k].copy_from_slice(user);
            have = true;
            let value = self.iter.value().unwrap_or(&[]);
            self.bytes += (raw.len() + value.len()) as u64;
            match block {
                None => self.sentinels += 1,
                Some(block) if block > self.head => self.above_head += 1,
                Some(block) => {
                    // The newest come last in a source sorted oldest first.
                    if oldest_first && self.versions.len() >= cap.max(1).saturating_mul(2) {
                        let excess = self.versions.len() - cap;
                        self.versions.drain(..excess);
                    }
                    if oldest_first || self.versions.len() < cap {
                        self.versions.push(Rec {
                            key,
                            block,
                            val: version_value(self.storage, value)?,
                        });
                    }
                }
            }
            self.iter.next();
        }
        self.iter.status()?;
        if oldest_first {
            let excess = self.versions.len().saturating_sub(cap);
            self.versions.drain(..excess);
        } else {
            self.versions.reverse();
        }
        Ok(have)
    }
}

/// Train one dictionary for a kind from keys spread over the key space.
fn train_kind_dict(
    src: &DB,
    schema: ArchiveSchema,
    storage: bool,
    head: u64,
) -> Result<Vec<u8>, Error> {
    let mut ro = ReadOptions::default();
    ro.set_total_order_seek(true);
    ro.set_verify_checksums(false);
    ro.fill_cache(false);
    let mut keys = KeyVersions::new(src, schema, storage, ro, head);
    let mut samples: Vec<Vec<Rec>> = Vec::new();
    let k = key_len(storage);
    for i in 0..DICT_SAMPLES {
        // Evenly spaced first bytes; the keys are hashes, so that is evenly
        // spaced data.
        let mut probe = vec![0u8; k];
        probe[..8].copy_from_slice(&(i * (u64::MAX / DICT_SAMPLES)).to_be_bytes());
        keys.iter.seek(&probe);
        let mut sample: Vec<Rec> = Vec::new();
        // About two pages of consecutive keys.
        while sample.len() < 1500 && keys.next_key(1024)? {
            sample.extend_from_slice(&keys.versions);
        }
        if !sample.is_empty() {
            samples.push(sample);
        }
    }
    Ok(file::train_dict(storage, samples.iter().map(Vec::as_slice)))
}

/// Writes the base files of one shard, cut at key boundaries by size.
struct ShardWriter<'a> {
    dir: &'a Path,
    storage: bool,
    dict: &'a Dict,
    level: i32,
    file_bytes: u64,
    head: u64,
    next_fid: &'a AtomicU32,
    writer: Option<(u32, String, PageWriter)>,
    done: Vec<NewBase>,
    bytes: u64,
}

impl ShardWriter<'_> {
    /// Add versions of one key, oldest first: all of them, or the next of
    /// the key added last.
    fn add_key(&mut self, versions: &[Rec]) -> Result<(), Error> {
        let Some(first) = versions.first() else {
            return Ok(());
        };
        let k = key_len(self.storage);
        // A key's versions never span files.
        if self
            .writer
            .as_ref()
            .is_some_and(|w| w.2.bytes() >= self.file_bytes && w.2.can_cut_before(&first.key[..k]))
        {
            self.close()?;
        }
        if self.writer.is_none() {
            let fid = self.next_fid.fetch_add(1, Ordering::Relaxed);
            let name = format!("base-{fid:08}-{}", if self.storage { 's' } else { 'a' });
            let w = PageWriter::new(
                &self.dir.join(&name),
                self.storage,
                self.dict,
                self.level,
                None,
            )?;
            self.writer = Some((fid, name, w));
        }
        let w = &mut self.writer.as_mut().unwrap().2;
        for r in versions {
            debug_assert_eq!(r.key, first.key);
            w.add(&r.key[..k], r.block, &r.val)?;
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), Error> {
        let Some((fid, name, w)) = self.writer.take() else {
            return Ok(());
        };
        let out = w.finish()?;
        self.bytes += out.bytes;
        self.done.push(NewBase {
            fid,
            storage: self.storage,
            name,
            dict: (!self.dict.raw.is_empty())
                .then(|| DICT_NAMES[self.storage as usize].to_string()),
            hi: self.head,
            records: out.records,
            first_key: out.first_key.expect("a closed file has records"),
        });
        Ok(())
    }
}

impl DataBaseRef {
    /// Offline rewrite of a RocksDB archive (schema v1, v2 or v3, told by
    /// its schema file and its keys) into schema v4 at `dst_path`, which
    /// must not exist or be empty. The source is opened read-only.
    ///
    /// The versioned column families are read shard by shard and written as
    /// v4 base files holding every version up to the source's head; the
    /// result has no delta. Block infos and code go to the files of the
    /// store. The schema file is written last: an interrupted conversion
    /// cannot be opened as v4.
    pub fn convert_to_v4(
        src_path: &Path,
        dst_path: &Path,
        jobs: usize,
    ) -> Result<V4ConvertReport, Error> {
        if dst_path.exists() && std::fs::read_dir(dst_path)?.next().is_some() {
            return Err(Error::UnSupported(format!(
                "destination {} is not empty",
                dst_path.display()
            )));
        }
        let jobs = jobs.clamp(1, 64);
        let (src, schema) = open_source(src_path)?;
        let head = source_head(&src)?;
        if head > u32::MAX as u64 {
            return Err(Error::UnSupported(format!(
                "archive schema v4 holds block numbers up to 2^32, not {head}"
            )));
        }
        info!(target: "migrate", "v4 convert: source is schema {schema}, its head is block {head}");

        let store_dir = dst_path.join(STORE_DIR);
        std::fs::create_dir_all(&store_dir)?;
        side::create(&store_dir)?;

        let start = std::time::Instant::now();
        let options = Options::maintenance(0);
        let mut dicts = Vec::new();
        for storage in [false, true] {
            let raw = train_kind_dict(&src, schema, storage, head)?;
            info!(
                target: "migrate",
                "v4 convert: {} dictionary of {} bytes",
                if storage { "storage" } else { "account" },
                raw.len()
            );
            if !raw.is_empty() {
                file::write_file_synced(&store_dir.join(DICT_NAMES[storage as usize]), &raw)?;
            }
            dicts.push(Dict::new(0, raw));
        }

        // Block infos and code first, as each runs on one thread, then the
        // storage shards: they are the bulk of the work.
        let mut tasks = vec![Task::Blocks, Task::Codes];
        for storage in [true, false] {
            for (lo, hi) in shard_bounds(4 * jobs) {
                tasks.push(Task::Versioned { storage, lo, hi });
            }
        }
        let n_tasks = tasks.len();
        let queue = Mutex::new(tasks.into_iter());
        let done_tasks = AtomicU64::new(0);
        // Storage versions, account versions, other records, above the head,
        // pointers to the latest version.
        let records = [const { AtomicU64::new(0) }; 5];
        let bytes_read = AtomicU64::new(0);
        let bytes_written = AtomicU64::new(0);
        let next_fid = AtomicU32::new(1);
        let stop = AtomicBool::new(false);
        let bases: Mutex<Vec<NewBase>> = Mutex::new(Vec::new());
        let blocks = Blocks::open(&store_dir, BlocksMeta::default(), 1 << 20)?;
        let codes = Codes::open(&store_dir, CodesMeta::default())?;
        {
            let samples = sample_blocks(&src, head)?;
            let infos: Vec<&[u8]> = samples.iter().map(|info| &info[..]).collect();
            if let Some(dict) = blocks.train(&infos)? {
                blocks.publish(dict)?;
                info!(target: "migrate", "v4 convert: block dictionary from {} blocks", infos.len());
            }
            let samples = sample_codes(&src)?;
            let sampled: Vec<&[u8]> = samples.iter().map(|code| &code[..]).collect();
            if let Some(dict) = codes.train(&sampled)? {
                codes.publish(dict);
                info!(target: "migrate", "v4 convert: code dictionary from {} codes", sampled.len());
            }
        }

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
                        "v4 convert: {}/{} tasks, {} storage + {} account versions, {} other records, \
                         {:.1} GiB read ({:.0} MiB/s), {:.1} GiB written, elapsed {}",
                        done_tasks.load(Ordering::Relaxed), n_tasks,
                        records[0].load(Ordering::Relaxed), records[1].load(Ordering::Relaxed),
                        records[2].load(Ordering::Relaxed), read as f64 / (1u64 << 30) as f64,
                        read as f64 / (1u64 << 20) as f64 / secs,
                        bytes_written.load(Ordering::Relaxed) as f64 / (1u64 << 30) as f64,
                        fmt_hms(secs));
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
                            match task {
                                Task::Blocks => {
                                    read_blocks(
                                        &src,
                                        0,
                                        head,
                                        BLOCK_BATCH,
                                        &stop,
                                        &mut |first, recs| {
                                            records[2]
                                                .fetch_add(recs.len() as u64, Ordering::Relaxed);
                                            blocks.publish(blocks.append(
                                                first,
                                                &recs,
                                                side::LEVEL,
                                            )?)
                                        },
                                    )?;
                                }
                                Task::Codes => {
                                    read_codes(&src, &stop, &mut |batch| {
                                        records[2].fetch_add(batch.len() as u64, Ordering::Relaxed);
                                        codes.publish(codes.append(
                                            batch.iter().map(|(hash, code)| (hash, &code[..])),
                                            side::LEVEL_SLOW,
                                        )?);
                                        Ok(())
                                    })?;
                                }
                                Task::Versioned { storage, lo, hi } => {
                                    let mut keys = KeyVersions::new(
                                        &src,
                                        schema,
                                        storage,
                                        scan_options(Some(lo), hi),
                                        head,
                                    );
                                    keys.iter.seek([lo]);
                                    let mut out = ShardWriter {
                                        dir: &store_dir,
                                        storage,
                                        dict: &dicts[storage as usize],
                                        level: options.base_level,
                                        file_bytes: options.base_file_bytes,
                                        head,
                                        next_fid: &next_fid,
                                        writer: None,
                                        done: Vec::new(),
                                        bytes: 0,
                                    };
                                    let (mut seen, mut written) = (0u64, 0u64);
                                    while keys.drain_key(|versions| {
                                        records[usize::from(!storage)]
                                            .fetch_add(versions.len() as u64, Ordering::Relaxed);
                                        out.add_key(versions)
                                    })? {
                                        if keys.bytes - seen >= 64 << 20 {
                                            bytes_read
                                                .fetch_add(keys.bytes - seen, Ordering::Relaxed);
                                            seen = keys.bytes;
                                            let now = out.bytes
                                                + out.writer.as_ref().map_or(0, |w| w.2.bytes());
                                            bytes_written
                                                .fetch_add(now - written, Ordering::Relaxed);
                                            written = now;
                                            if stop.load(Ordering::Relaxed) {
                                                return Ok(());
                                            }
                                        }
                                    }
                                    out.close()?;
                                    bytes_read.fetch_add(keys.bytes - seen, Ordering::Relaxed);
                                    bytes_written.fetch_add(out.bytes - written, Ordering::Relaxed);
                                    records[3].fetch_add(keys.above_head, Ordering::Relaxed);
                                    records[4].fetch_add(keys.sentinels, Ordering::Relaxed);
                                    bases.lock().unwrap().append(&mut out.done);
                                }
                            }
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

        let mut report = V4ConvertReport {
            accounts: records[1].load(Ordering::Relaxed),
            storage: records[0].load(Ordering::Relaxed),
            above_head: records[3].load(Ordering::Relaxed),
            sentinels: records[4].load(Ordering::Relaxed),
            other: records[2].load(Ordering::Relaxed),
            files: 0,
            bytes: bytes_written.load(Ordering::Relaxed),
            head,
        };
        if report.above_head > 0 {
            warn!(
                target: "migrate",
                "v4 convert: left behind {} versions above the head (block {head})",
                report.above_head
            );
        }
        if blocks.meta().last() != Some(head) {
            return Err(Error::UnSupported(format!(
                "the source's block infos end at {:?}, its head is {head}",
                blocks.meta().last()
            )));
        }
        // The index from hash to number names the same blocks.
        let index_file = src_path.join(block_index::FILE);
        if index_file.is_file() {
            std::fs::copy(&index_file, dst_path.join(block_index::FILE))?;
        }
        let mut files = bases.into_inner().unwrap();
        // Shards are disjoint key ranges: the first keys order the files.
        files.sort_by(|a, b| (a.storage, a.first_key).cmp(&(b.storage, b.first_key)));
        report.files = files.len();
        Store::create_with_base(&store_dir, files, blocks.meta(), codes.meta())?;
        write_schema_file(dst_path, "4")?;
        info!(target: "migrate", "v4 convert: done in {}: {report:?}", fmt_hms(start.elapsed().as_secs_f64()));
        Ok(report)
    }
}

/// Blocks handed over at a time by [`read_blocks`] (each batch is written
/// and synced by itself), and codes by [`read_codes`].
const BLOCK_BATCH: usize = 1 << 16;
const CODE_BATCH: usize = 1 << 12;
/// Places of the chain whose block infos train the dictionary of the block
/// chunks, and blocks read at each.
const BLOCK_SAMPLES: u64 = 4000;
const BLOCK_SAMPLE_RUN: usize = 4;

/// Block infos spread over the chain up to `head`.
fn sample_blocks(src: &DB, head: u64) -> Result<Vec<Arc<[u8]>>, Error> {
    let mut infos = Vec::new();
    for i in 0..BLOCK_SAMPLES.min(head + 1) {
        let stop = AtomicBool::new(false);
        let from = (head as u128 * i as u128 / BLOCK_SAMPLES as u128) as u64;
        read_blocks(src, from, head, BLOCK_SAMPLE_RUN, &stop, &mut |_, recs| {
            infos.extend(
                recs.into_iter()
                    .map(|rec| rec.1)
                    .filter(|info| !info.is_empty()),
            );
            stop.store(true, Ordering::Relaxed);
            Ok(())
        })?;
    }
    Ok(infos)
}

/// The block infos of a RocksDB archive from block `from` (or its first
/// block, if that is later) up to `head`, in order and without gaps: a
/// block the archive does not have is handed over with a zero hash. `sink`
/// gets the number of the first block of each batch. Returns the blocks
/// handed over.
fn read_blocks(
    src: &DB,
    from: u64,
    head: u64,
    batch_blocks: usize,
    stop: &AtomicBool,
    sink: &mut dyn FnMut(u64, Vec<BlockRec>) -> Result<(), Error>,
) -> Result<u64, Error> {
    let absent: BlockRec = ([0u8; 32], Arc::from(&[][..]));
    let mut batch: Vec<BlockRec> = Vec::with_capacity(batch_blocks);
    // The number of the batch's first block, once a block was read.
    let mut first: Option<u64> = None;
    let mut total = 0u64;
    let mut put = |number: u64, rec: BlockRec| -> Result<bool, Error> {
        if number > head {
            return Ok(false);
        }
        let start = *first.get_or_insert(number);
        let next = start + batch.len() as u64;
        if number < next {
            return Err(Error::UnSupported(format!(
                "block {number} of the source follows block {}",
                next - 1
            )));
        }
        for _ in next..number {
            batch.push(absent.clone());
        }
        batch.push(rec);
        if batch.len() >= batch_blocks {
            total += batch.len() as u64;
            let len = batch.len() as u64;
            sink(
                start,
                std::mem::replace(&mut batch, Vec::with_capacity(batch_blocks)),
            )?;
            first = Some(start + len);
        }
        Ok(!stop.load(Ordering::Relaxed))
    };
    let mut ro = scan_options(None, None);
    if let Some(cf) = src.cf_handle(StorageTypeColumn::BlockNumToBlockInfo.to_str()) {
        // By number: the hash leads the value.
        ro.set_iterate_lower_bound(block_index::key(from).to_vec());
        for item in src.iterator_cf_opt(cf, ro, IteratorMode::Start) {
            let (key, value) = item?;
            if key.len() != 8 || value.len() < 32 {
                return Err(Error::UnSupported("malformed block info by number".into()));
            }
            let number = u64::from_be_bytes(key[..].try_into().unwrap());
            let hash: [u8; 32] = value[..32].try_into().unwrap();
            if !put(number, (hash, Arc::from(&value[32..])))? {
                break;
            }
        }
    } else {
        // By hash: the numbers name the hashes, in order.
        let numbers = src
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .ok_or_else(|| Error::UnSupported("source archive has no block numbers".into()))?;
        let infos = src
            .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
            .ok_or_else(|| Error::UnSupported("source archive has no block infos".into()))?;
        ro.set_iterate_lower_bound(super::super::encode_block_num(from).to_vec());
        let mut pending: Vec<(u64, [u8; 32])> = Vec::with_capacity(1024);
        let mut flush = |pending: &mut Vec<(u64, [u8; 32])>| -> Result<bool, Error> {
            let found = src.multi_get_cf(pending.iter().map(|(_, hash)| (infos, hash)));
            for ((number, hash), info) in pending.drain(..).zip(found) {
                let info = info?.unwrap_or_default();
                if !put(number, (hash, Arc::from(info)))? {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        let mut more = true;
        for item in src.iterator_cf_opt(numbers, ro, IteratorMode::Start) {
            let (key, hash) = item?;
            if key.len() != 32 || hash.len() != 32 {
                return Err(Error::UnSupported("malformed block number record".into()));
            }
            let number = u64::from_be_bytes(key[24..].try_into().unwrap());
            pending.push((number, hash[..].try_into().unwrap()));
            if pending.len() == 1024 && !flush(&mut pending)? {
                more = false;
                break;
            }
        }
        if more {
            flush(&mut pending)?;
        }
    }
    if let Some(start) = first.filter(|_| !batch.is_empty()) {
        total += batch.len() as u64;
        sink(start, batch)?;
    }
    Ok(total)
}

/// Places of the hash space whose codes train the dictionary of the code,
/// and codes read at each.
const CODE_SAMPLES: u64 = 4000;
const CODE_SAMPLE_RUN: usize = 2;

/// Codes spread over a RocksDB archive.
fn sample_codes(src: &DB) -> Result<Vec<Box<[u8]>>, Error> {
    let Some(cf) = src.cf_handle(StorageTypeColumn::HashToCode.to_str()) else {
        return Ok(Vec::new());
    };
    let mut codes = Vec::new();
    let mut ro = ReadOptions::default();
    ro.set_total_order_seek(true);
    ro.fill_cache(false);
    let mut iter = src.raw_iterator_cf_opt(cf, ro);
    for i in 0..CODE_SAMPLES {
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(&(i * (u64::MAX / CODE_SAMPLES)).to_be_bytes());
        iter.seek(key);
        for _ in 0..CODE_SAMPLE_RUN {
            let Some(code) = iter.value() else {
                break;
            };
            codes.push(Box::from(code));
            iter.next();
        }
        iter.status()?;
    }
    Ok(codes)
}

/// Every code of a RocksDB archive, in batches.
fn read_codes(
    src: &DB,
    stop: &AtomicBool,
    sink: &mut dyn FnMut(Vec<([u8; 32], Box<[u8]>)>) -> Result<(), Error>,
) -> Result<u64, Error> {
    let Some(cf) = src.cf_handle(StorageTypeColumn::HashToCode.to_str()) else {
        return Ok(0);
    };
    let mut batch = Vec::with_capacity(CODE_BATCH);
    let mut total = 0u64;
    for item in src.iterator_cf_opt(cf, scan_options(None, None), IteratorMode::Start) {
        let (hash, code) = item?;
        let hash: [u8; 32] = hash[..]
            .try_into()
            .map_err(|_| Error::UnSupported("malformed code hash".into()))?;
        batch.push((hash, code));
        if batch.len() >= CODE_BATCH {
            total += batch.len() as u64;
            sink(std::mem::replace(
                &mut batch,
                Vec::with_capacity(CODE_BATCH),
            ))?;
            if stop.load(Ordering::Relaxed) {
                return Ok(total);
            }
        }
    }
    total += batch.len() as u64;
    sink(batch)?;
    Ok(total)
}

/// Block `number` of a RocksDB archive as [`read_blocks`] hands it over, if
/// the archive has it.
fn source_block(src: &DB, number: u64) -> Result<Option<BlockRec>, Error> {
    if let Some(cf) = src.cf_handle(StorageTypeColumn::BlockNumToBlockInfo.to_str()) {
        let Some(value) = src.get_cf(cf, block_index::key(number))? else {
            return Ok(None);
        };
        if value.len() < 32 {
            return Err(Error::UnSupported("malformed block info by number".into()));
        }
        let hash: [u8; 32] = value[..32].try_into().unwrap();
        return Ok(Some((hash, Arc::from(&value[32..]))));
    }
    let numbers = src
        .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
        .ok_or_else(|| Error::UnSupported("source archive has no block numbers".into()))?;
    let infos = src
        .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
        .ok_or_else(|| Error::UnSupported("source archive has no block infos".into()))?;
    let Some(hash) = src.get_cf(numbers, encode_block_num(number))? else {
        return Ok(None);
    };
    let hash: [u8; 32] = hash[..]
        .try_into()
        .map_err(|_| Error::UnSupported("malformed block number record".into()))?;
    let info = src.get_cf(infos, hash)?.unwrap_or_default();
    Ok(Some((hash, Arc::from(info))))
}

/// How far below its last block info the head of a source is looked for.
const HEAD_SEARCH_BLOCKS: usize = 1 << 20;

/// The number of the latest block of an archive opened read-only.
fn source_head(src: &DB) -> Result<u64, Error> {
    let meta = src
        .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
        .ok_or_else(|| Error::UnSupported("source archive has no head".into()))?;
    let hash = src
        .get_cf(meta, [1u8])?
        .ok_or_else(|| Error::UnSupported("source archive is empty".into()))?;
    source_block_number(src, &hash)
}

/// The number of the block with hash `hash` in an archive opened read-only.
fn source_block_number(db: &DB, hash: &[u8]) -> Result<u64, Error> {
    if let Some(cf) = db.cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str()) {
        if let Some(info) = db.get_cf(cf, hash)? {
            return Ok(decode_archive_header(&mut &info[..])?.number);
        }
    }
    // Infos by number: the head is the last record that names its hash. A
    // head-only rewind leaves the infos of the blocks it went back over.
    let cf = db
        .cf_handle(StorageTypeColumn::BlockNumToBlockInfo.to_str())
        .ok_or_else(|| Error::UnSupported("source archive has no block infos".into()))?;
    let mut ro = ReadOptions::default();
    ro.set_total_order_seek(true);
    let mut iter = db.raw_iterator_cf_opt(cf, ro);
    iter.seek_to_last();
    for _ in 0..HEAD_SEARCH_BLOCKS {
        match (iter.key(), iter.value()) {
            (Some(key), Some(value)) if key.len() == 8 && value.len() >= 32 => {
                if value[..32] == *hash {
                    return Ok(u64::from_be_bytes(key.try_into().unwrap()));
                }
            }
            _ => break,
        }
        iter.prev();
    }
    iter.status()?;
    Err(Error::UnSupported(format!(
        "the source's head is not among its last {HEAD_SEARCH_BLOCKS} block infos"
    )))
}

// ---------------------------------------------------------------------------
// Verification against the source
// ---------------------------------------------------------------------------

/// Result of [`verify_v4_against_source`].
#[derive(Debug, Default, Clone)]
pub struct V4VerifyReport {
    pub keys: u64,
    /// Storage keys among them that are not the first slot of their
    /// address.
    pub inner_slots: u64,
    pub reads: u64,
    pub absent_keys: u64,
    /// Block numbers and codes compared.
    pub blocks: u64,
    pub codes: u64,
    pub mismatches: u64,
}

/// Versions of one key kept for a check; hotter keys are checked over their
/// newest versions only.
const VERIFY_MAX_VERSIONS: usize = 4096;

/// Compare a v4 archive with the RocksDB archive (v1, v2 or v3) it was
/// converted from: for `samples` random keys of each kind, read v4 at
/// heights on, just below and between the key's versions and above the
/// newest, and compare value and block with the source's; also a key next
/// to each that the source does not hold. Heights go up to the last block
/// the v4 archive's files hold. Then `samples` block numbers up to that
/// block (hash and info, or that neither has the block) and `samples`
/// codes of the source, and that v4 has no code under a hash the source
/// has none under. A code that v4 lacks is a mismatch unless the source is
/// ahead of it, when the code may be of a later block.
pub fn verify_v4_against_source(
    src_path: &Path,
    v4_path: &Path,
    samples: u64,
    seed: u64,
) -> Result<V4VerifyReport, Error> {
    if !udt::is_v4_archive(v4_path)? {
        return Err(Error::UnSupported(format!(
            "{} is not a schema v4 archive ({SCHEMA_FILE})",
            v4_path.display()
        )));
    }
    let (src, schema) = open_source(src_path)?;
    let store = Store::open(&v4_path.join(STORE_DIR), Options::maintenance(1 << 30))?;
    let head = store.head().unwrap_or(0);
    let mut rng = seed | 1;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut report = V4VerifyReport::default();
    for storage in [false, true] {
        let k = key_len(storage);
        let mut ro = ReadOptions::default();
        ro.set_total_order_seek(true);
        // The source may be ahead of the converted archive.
        let mut keys = KeyVersions::new(&src, schema, storage, ro, head);
        for i in 0..samples {
            if i > 0 && i % 5000 == 0 {
                info!(target: "migrate", "v4 verify: {} {i}/{samples} keys, {report:?}",
                    if storage { "storage" } else { "account" });
            }
            let probe: Vec<u8> = (0..k).map(|_| next() as u8).collect();
            keys.iter.seek(&probe);
            if storage {
                // No address starts with random bytes: the seek lands on the
                // first slot of the next address, and would never reach the
                // other slots of a contract. Go on to a slot picked at
                // random in that address (or, past its last one, the first
                // slot of the address after it).
                if let Some(raw) = keys.iter.key() {
                    let first = version_key(schema, raw, k)?.0.to_vec();
                    let mut inner = first.clone();
                    for b in &mut inner[32..] {
                        *b = next() as u8;
                    }
                    keys.iter.seek(&inner);
                    let landed = keys
                        .iter
                        .key()
                        .map(|raw| version_key(schema, raw, k))
                        .transpose()?;
                    if landed.is_some_and(|(key, _)| key[..32] == first[..32] && key != &first[..])
                    {
                        report.inner_slots += 1;
                    }
                }
            }
            if !keys.next_key(VERIFY_MAX_VERSIONS)? {
                continue;
            }
            let versions = std::mem::take(&mut keys.versions);
            let Some(first) = versions.first() else {
                continue;
            };
            let capped = versions.len() == VERIFY_MAX_VERSIONS;
            let key = first.key;
            report.keys += 1;
            let oldest = first.block;
            let newest = versions.last().unwrap().block;
            let mut heights = vec![newest + 1 + next() % 1000, u32::MAX as u64];
            for _ in 0..8 {
                let b = versions[(next() % versions.len() as u64) as usize].block;
                heights.push(b);
                if b > 0 && (!capped || b > oldest) {
                    heights.push(b - 1);
                }
                let lo = if capped { oldest } else { 0 };
                heights.push(lo + next() % (newest - lo + 2));
            }
            for h in heights {
                let i = versions.partition_point(|r| r.block <= h);
                let want = (i > 0).then(|| (versions[i - 1].block, versions[i - 1].val));
                let read = store.get(storage, &key[..k], h)?;
                report.reads += 1;
                // The span the read claims must not skip a version either.
                let next_version = versions.get(i).map_or(u64::MAX, |r| r.block);
                if read.found != want || read.next <= h || read.next > next_version {
                    report.mismatches += 1;
                    if report.mismatches <= 20 {
                        warn!(target: "migrate",
                            "v4 verify mismatch: key {} at {h}: source {want:?} (next version {next_version}), v4 {read:?}",
                            hex(&key[..k]));
                    }
                }
            }
            // A key next to it that the source does not hold.
            let mut absent = key;
            absent[k - 1] ^= 0x5a;
            keys.iter.seek(&absent[..k]);
            let held = match keys.iter.key() {
                Some(raw) => version_key(schema, raw, k)?.0 == &absent[..k],
                None => {
                    keys.iter.status()?;
                    false
                }
            };
            if !held {
                report.absent_keys += 1;
                report.reads += 1;
                if store
                    .get(storage, &absent[..k], u32::MAX as u64)?
                    .found
                    .is_some()
                {
                    report.mismatches += 1;
                    warn!(target: "migrate", "v4 verify: key {} absent in the source but found", hex(&absent[..k]));
                }
            }
        }
    }
    let Some(head) = store.head() else {
        return Ok(report);
    };
    let mismatch = |report: &mut V4VerifyReport, what: String| {
        report.mismatches += 1;
        if report.mismatches <= 20 {
            warn!(target: "migrate", "v4 verify mismatch: {what}");
        }
    };
    for i in 0..samples {
        if i > 0 && i % 5000 == 0 {
            info!(target: "migrate", "v4 verify: block {i}/{samples}, {report:?}");
        }
        let number = next() % (head + 1);
        let want = source_block(&src, number)?;
        report.blocks += 1;
        if store.block(number)? != want || store.block_hash(number)? != want.map(|rec| rec.0) {
            mismatch(&mut report, format!("block {number}"));
        }
    }
    if let Some(cf) = src.cf_handle(StorageTypeColumn::HashToCode.to_str()) {
        let ahead = source_head(&src)?.saturating_sub(head);
        let mut later = 0u64;
        let mut ro = ReadOptions::default();
        ro.set_total_order_seek(true);
        let mut iter = src.raw_iterator_cf_opt(cf, ro);
        for i in 0..samples {
            if i > 0 && i % 5000 == 0 {
                info!(target: "migrate", "v4 verify: code {i}/{samples}, {report:?}");
            }
            let probe: [u8; 32] = std::array::from_fn(|_| next() as u8);
            iter.seek(probe);
            let (Some(hash), Some(code)) = (iter.key(), iter.value()) else {
                iter.status()?;
                continue;
            };
            let hash: [u8; 32] = hash
                .try_into()
                .map_err(|_| Error::UnSupported("malformed code hash".into()))?;
            match store.code(&hash)? {
                None if ahead > 0 => later += 1,
                read => {
                    report.codes += 1;
                    if read.as_deref() != Some(code) {
                        mismatch(&mut report, format!("code {}", hex(&hash)));
                    }
                }
            }
            if hash != probe && store.code(&probe)?.is_some() {
                mismatch(
                    &mut report,
                    format!("code {} absent in the source but found", hex(&probe)),
                );
            }
        }
        if later > 0 {
            info!(target: "migrate",
                "v4 verify: {later} codes of the source are not in v4; the source is {ahead} blocks ahead");
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Replay of a captured lookup trace
// ---------------------------------------------------------------------------

/// Result of one phase of [`replay_get_trace`].
#[derive(Debug, Default, Clone)]
pub struct V4ReplayReport {
    pub gets: u64,
    pub seconds: f64,
    /// Device reads and bytes read for the lookups.
    pub reads: u64,
    pub read_bytes: u64,
    /// Seconds of device read time, summed over the threads.
    pub read_seconds: f64,
    /// CPU seconds of the process (user and system).
    pub cpu_seconds: f64,
    /// Latency percentiles in microseconds: 50, 90, 99, 99.9.
    pub latency_us: [u32; 4],
    /// Lookups whose version block differs from the trace's.
    pub mismatches: u64,
}

fn cpu_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage fills the struct it is given.
    let usage = unsafe {
        libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
        usage.assume_init()
    };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    secs(usage.ru_utime) + secs(usage.ru_stime)
}

/// Replay a trace of archive lookups against the store of the v4 archive at
/// `v4_path` (what its files hold), on
/// `threads` threads, and compare the block of each version found with the
/// one the trace recorded. The first `warmup` lookups fill the cache and are
/// reported apart. A trace record is
/// `cf u8 (4 account, 5 storage) | key length u8 | key | height u64 LE |
/// found block u64 LE (u64::MAX: none)`.
pub fn replay_get_trace(
    v4_path: &Path,
    trace: &Path,
    threads: usize,
    cache_mb: u64,
    warmup: u64,
) -> Result<[V4ReplayReport; 2], Error> {
    let bytes = std::fs::read(trace)?;
    let mut offsets: Vec<u64> = Vec::new();
    let mut at = 0usize;
    while at + 18 <= bytes.len() {
        let kl = bytes[at + 1] as usize;
        let ok = matches!((bytes[at], kl), (4, 32) | (5, 64)) && at + 18 + kl <= bytes.len();
        if !ok {
            return Err(Error::UnSupported(format!(
                "malformed trace record at byte {at}"
            )));
        }
        offsets.push(at as u64);
        at += 18 + kl;
    }
    info!(target: "migrate", "v4 replay: {} lookups loaded", offsets.len());
    let store = Store::open(
        &v4_path.join(STORE_DIR),
        Options::maintenance(cache_mb << 20),
    )?;
    let head = store.head().unwrap_or(0);
    let warmup = (warmup as usize).min(offsets.len());
    let phase = |from: usize, to: usize| -> Result<V4ReplayReport, Error> {
        let next = std::sync::atomic::AtomicUsize::new(from);
        let (reads0, bytes0, nanos0) = super::io_stats().snapshot();
        let (cpu0, started) = (cpu_seconds(), std::time::Instant::now());
        let results: Vec<Result<(Vec<u32>, u64), Error>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads.max(1))
                .map(|_| {
                    scope.spawn(|| -> Result<(Vec<u32>, u64), Error> {
                        let (mut lat, mut mismatches) = (Vec::new(), 0u64);
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= to {
                                return Ok((lat, mismatches));
                            }
                            let at = offsets[i] as usize;
                            let storage = bytes[at] == 5;
                            let k = key_len(storage);
                            let key = &bytes[at + 2..at + 2 + k];
                            let h = u64::from_le_bytes(bytes[at + 2 + k..at + 10 + k].try_into().unwrap());
                            let want = u64::from_le_bytes(bytes[at + 10 + k..at + 18 + k].try_into().unwrap());
                            // The trace may reach past what this archive holds.
                            if h > head || (want != u64::MAX && want > head) {
                                continue;
                            }
                            let t = std::time::Instant::now();
                            let read = store.get(storage, key, h)?;
                            lat.push(t.elapsed().as_micros().min(u32::MAX as u128) as u32);
                            let got = read.found.map_or(u64::MAX, |f| f.0);
                            if got != want || read.next <= h {
                                mismatches += 1;
                                if mismatches <= 3 {
                                    warn!(target: "migrate",
                                        "v4 replay mismatch: lookup {i}, key {} at {h}: trace {want}, v4 {read:?}",
                                        hex(key));
                                }
                            }
                        }
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().expect("replay thread panicked"))
                .collect()
        });
        let seconds = started.elapsed().as_secs_f64();
        let cpu_seconds = cpu_seconds() - cpu0;
        let (reads1, bytes1, nanos1) = super::io_stats().snapshot();
        let mut lat: Vec<u32> = Vec::new();
        let mut mismatches = 0;
        for r in results {
            let (l, m) = r?;
            lat.extend(l);
            mismatches += m;
        }
        lat.sort_unstable();
        let pct = |p: f64| {
            lat.get(((lat.len() as f64 - 1.0) * p) as usize)
                .copied()
                .unwrap_or(0)
        };
        Ok(V4ReplayReport {
            gets: lat.len() as u64,
            seconds,
            reads: reads1 - reads0,
            read_bytes: bytes1 - bytes0,
            read_seconds: (nanos1 - nanos0) as f64 / 1e9,
            cpu_seconds,
            latency_us: [pct(0.5), pct(0.9), pct(0.99), pct(0.999)],
            mismatches,
        })
    };
    let warm = phase(0, warmup)?;
    info!(target: "migrate", "v4 replay: warm-up {warm:?}");
    let measured = phase(warmup, offsets.len())?;
    info!(target: "migrate", "v4 replay: measured {measured:?}; store sizes {:?}", store.sizes());
    Ok([warm, measured])
}

#[cfg(test)]
mod tests {
    use super::super::super::udt::{init_archive_schema, ArchiveSchema};
    use super::super::super::*;
    use super::*;
    use crate::db::{StateDBRead, StateDBWrapper};
    use crate::db_impl::set_inverted_block_encoding;
    use crate::interface::EvmStorageWrite;
    use leafage_evm_types::{AccountStorageDiff, BlockStorageDiff, IndexValuePair};

    fn block_info(number: u64) -> BlockInfo {
        let mut hash = H256::ZERO;
        hash.0[24..].copy_from_slice(&number.to_be_bytes());
        hash.0[0] = 0xb1;
        BlockInfo::new(Block {
            header: Header {
                hash,
                inner: RawHeader {
                    number,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn word(seed: u64) -> H256 {
        let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut h = H256::ZERO;
        for chunk in h.0.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            chunk.copy_from_slice(&x.to_be_bytes());
        }
        h
    }

    fn diff(n: u64) -> BlockStorageDiff {
        let mut d = BlockStorageDiff::default();
        for a in 0..40u64 {
            if (a + n) % 7 == 0 && n > 2 {
                d.deleted_accounts.push(word(a));
                continue;
            }
            if (a * 3 + n) % 4 != 0 {
                continue;
            }
            d.new_accounts.push(NewAccount {
                address: word(a),
                balance: U256::from(n * 1000 + a),
                nonce: n,
                code_hash: if a % 2 == 0 {
                    KECCAK256_EMPTY.0.into()
                } else {
                    word(900 + a)
                },
            });
            d.storage_diffs.push(AccountStorageDiff {
                address: word(a),
                diffs: (0..if a == 1 { 300 } else { 3 })
                    .filter(|s| (s + n) % 3 != 1)
                    .map(|s| IndexValuePair {
                        index: word(10_000 + a * 1000 + s),
                        value: if (s + n) % 5 == 0 {
                            U256::ZERO
                        } else {
                            U256::from(n) << ((s % 4) * 60) as usize
                        },
                    })
                    .collect(),
            });
        }
        d
    }

    /// What a node on the archive answers at heights up to `head`.
    fn read_all(db: &Arc<DataBaseRef>, head: u64) -> Vec<String> {
        let mut out = Vec::new();
        for h in (1..=head).step_by(3).chain([head]) {
            let state = db
                .db_at(BlockId::Number(BlockNumberOrTag::Number(h)))
                .unwrap()
                .unwrap();
            for a in 0..42u64 {
                out.push(format!("{:?}", state.read_account(word(a)).unwrap()));
                for s in (0..if a == 1 { 302 } else { 4 }).step_by(if a == 1 { 17 } else { 1 }) {
                    out.push(format!(
                        "{:?}",
                        state
                            .read_storage_and_height(word(a), word(10_000 + a * 1000 + s))
                            .unwrap()
                    ));
                }
            }
            out.push(format!("{:?}", state.read_storage_slots(word(1)).unwrap()));
        }
        out
    }

    fn append(db: &Arc<DataBaseRef>, blocks: std::ops::RangeInclusive<u64>) {
        for n in blocks {
            let state = StateDBWrapper(
                db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                    .unwrap()
                    .unwrap(),
            );
            state.update_block(block_info(n), diff(n)).unwrap();
        }
    }

    /// The base files of the v4 archive at `dir`, by name.
    fn base_files(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(dir.join(STORE_DIR))
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.unwrap();
                let name = entry.file_name().into_string().unwrap();
                name.starts_with("base-")
                    .then(|| (name, std::fs::read(entry.path()).unwrap()))
            })
            .collect();
        files.sort();
        files
    }

    /// Every key of a kind in the source at `dir` with its versions, at
    /// most `cap` of each.
    fn source_versions(dir: &Path, storage: bool, cap: usize) -> Vec<Vec<(u64, String)>> {
        let (src, schema) = open_source(dir).unwrap();
        let mut keys = KeyVersions::new(&src, schema, storage, scan_options(None, None), 60);
        keys.iter.seek_to_first();
        let mut out = Vec::new();
        while keys.next_key(cap).unwrap() {
            if let Some(first) = keys.versions.first() {
                let key = hex(&first.key[..key_len(storage)]);
                out.push(
                    keys.versions
                        .iter()
                        .map(|r| (r.block, format!("{key} {:?}", r.val)))
                        .collect(),
                );
            }
        }
        out
    }

    /// The same blocks in a v1, a v2 and a v3 archive convert to the same
    /// v4 archive, which answers as each of them.
    #[test]
    fn a_source_of_any_rocksdb_schema_converts_alike() {
        use crate::db_impl::archive_encoding::{encode_account_key, encode_storage_key};
        let _g = ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("leafage-v4-sources-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut converted = Vec::new();
        for schema in [ArchiveSchema::V1, ArchiveSchema::V2, ArchiveSchema::V3] {
            let (src, dst) = (
                base.join(schema.to_string()),
                base.join(format!("{schema}-v4")),
            );
            set_inverted_block_encoding(schema.inverted());
            init_archive_schema(&src, schema).unwrap();
            let (want, sentinels) = {
                let db = Arc::new(DataBaseRef::open(&src, 64, false, false));
                // A v1 archive from before the marker has none.
                if schema.inverted() {
                    db.write_encoding_marker(true).unwrap();
                }
                append(&db, 1..=60);
                let mut batch = db.prepare_write_batch().unwrap();
                db.write_code(&mut batch, word(901), Bytes::from(vec![1, 2, 3]))
                    .unwrap();
                db.commit(batch).unwrap();
                let want = read_all(&db, 60);
                // The pointers to the latest version that an early v1
                // archive holds: no versions, and here no values either.
                let mut sentinels = 0;
                if schema == ArchiveSchema::V1 {
                    let [accounts, slots] = [
                        StorageTypeColumn::AddressToAccount,
                        StorageTypeColumn::AddressToStorage,
                    ]
                    .map(|col| db.db.cf_handle(col.to_str()).unwrap());
                    for a in 0..40u64 {
                        let slot = word(10_000 + a * 1000);
                        db.db
                            .put_cf(accounts, encode_account_key(word(a), u64::MAX), [0xff; 5])
                            .unwrap();
                        db.db
                            .put_cf(
                                slots,
                                encode_storage_key(word(a), slot, u64::MAX),
                                [0xff; 40],
                            )
                            .unwrap();
                        sentinels += 2;
                    }
                }
                (want, sentinels)
            };

            let passes = || [0, 1].map(|i| KEY_PASSES[i].load(Ordering::Relaxed));
            let before = passes();
            // One worker: the files are numbered in the order of the keys.
            let report = DataBaseRef::convert_to_v4(&src, &dst, 1).unwrap();
            let [whole, windows] = [0, 1].map(|i| passes()[i] - before[i]);
            assert_eq!(
                (report.head, report.above_head, report.sentinels),
                (60, 0, sentinels),
                "{schema}"
            );
            // Keys with few versions and keys passed on in several windows.
            assert!(whole > 20 && windows > 100, "{schema}: {whole} {windows}");
            let files = base_files(&dst);

            let verify = verify_v4_against_source(&src, &dst, 300, 7).unwrap();
            assert_eq!(verify.mismatches, 0, "{schema}: {verify:?}");
            assert!(
                verify.keys > 100 && verify.absent_keys > 100 && verify.inner_slots > 0,
                "{schema}: {verify:?}"
            );
            // The newest versions of a key, when not all are wanted.
            let mut versions = Vec::new();
            for storage in [false, true] {
                let all = source_versions(&src, storage, usize::MAX);
                let newest = source_versions(&src, storage, 5);
                assert_eq!(all.len(), newest.len(), "{schema}");
                assert!(all.iter().any(|key| key.len() > 10), "{schema}");
                for (all, newest) in all.iter().zip(&newest) {
                    assert!(all.windows(2).all(|w| w[0].0 < w[1].0), "{schema}");
                    assert_eq!(&all[all.len().saturating_sub(5)..], &newest[..], "{schema}");
                }
                versions.push(all);
            }

            set_inverted_block_encoding(true);
            let db = Arc::new(DataBaseRef::open(&dst, 64, false, false));
            assert!(db.is_schema_v4());
            assert!(
                read_all(&db, 60) == want,
                "{schema}: reads of the converted archive"
            );
            for n in [1u64, 17, 60] {
                let info = db.read_block_info(block_info(n).header.hash).unwrap();
                assert_eq!(info.map(|info| info.header.number), Some(n));
            }
            assert_eq!(
                db.read_code(word(901)).unwrap(),
                Some(Bytes::from(vec![1, 2, 3]))
            );
            drop(db);
            converted.push((
                schema,
                (report.accounts, report.storage),
                want,
                versions,
                files,
            ));
        }
        let (_, records, reads, versions, files) = &converted[2];
        assert!(records.0 > 300 && records.1 > 3000 && files.len() >= 2);
        for (schema, r, w, v, f) in &converted[..2] {
            assert_eq!(r, records, "{schema}");
            assert!(w == reads, "{schema}: reads of the source");
            assert!(v == versions, "{schema}: versions of the source");
            assert_eq!(
                f.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                files.iter().map(|(name, _)| name).collect::<Vec<_>>(),
                "{schema}"
            );
            assert!(f == files, "{schema}: base files");
        }
        set_inverted_block_encoding(false);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A source without a schema file is v1 or v2 as its keys say, and is
    /// refused if its encoding marker says otherwise. A v4 archive is no
    /// source.
    #[test]
    fn a_source_is_told_v1_from_v2_by_its_keys() {
        let _g = ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("leafage-v4-schema-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for (i, (inverted, marker, want)) in [
            (false, None, Some(ArchiveSchema::V1)),
            (false, Some(false), Some(ArchiveSchema::V1)),
            (true, None, Some(ArchiveSchema::V2)),
            (true, Some(true), Some(ArchiveSchema::V2)),
            (false, Some(true), None),
            (true, Some(false), None),
        ]
        .into_iter()
        .enumerate()
        {
            let dir = base.join(i.to_string());
            set_inverted_block_encoding(inverted);
            {
                let db = Arc::new(DataBaseRef::open(&dir, 64, false, false));
                append(&db, 1..=5);
                if let Some(marker) = marker {
                    db.write_encoding_marker(marker).unwrap();
                }
            }
            let found = open_source(&dir).map(|(_, schema)| schema).ok();
            assert_eq!(found, want, "inverted {inverted}, marker {marker:?}");
            if want.is_none() {
                assert!(DataBaseRef::convert_to_v4(&dir, &base.join("refused"), 1).is_err());
            }
        }
        let v4 = base.join("v4");
        init_archive_schema(&v4, ArchiveSchema::V4).unwrap();
        assert!(open_source(&v4).is_err());
        set_inverted_block_encoding(false);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The head of a source that keeps its block infos by number is found
    /// below the infos a head-only rewind left above it.
    #[test]
    fn the_head_is_found_below_later_block_infos() {
        let dir = std::env::temp_dir().join(format!("leafage-v4-head-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let name = StorageTypeColumn::BlockNumToBlockInfo.to_str();
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let db = DB::open_cf(&opts, &dir, [name]).unwrap();
        let cf = db.cf_handle(name).unwrap();
        let hash = |n: u64| [n as u8; 32];
        for n in 1..=10u64 {
            let mut value = hash(n).to_vec();
            value.extend_from_slice(b"info");
            db.put_cf(cf, n.to_be_bytes(), value).unwrap();
        }
        assert_eq!(source_block_number(&db, &hash(10)).unwrap(), 10);
        assert_eq!(source_block_number(&db, &hash(7)).unwrap(), 7);
        assert!(source_block_number(&db, &hash(11)).is_err());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_converted_archive_answers_as_its_source() {
        let _g = ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("leafage-v4-convert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (src, dst) = (base.join("v3"), base.join("v4"));
        set_inverted_block_encoding(true);
        init_archive_schema(&src, ArchiveSchema::V3).unwrap();
        let want = {
            let db = Arc::new(DataBaseRef::open(&src, 64, false, false));
            db.write_encoding_marker(true).unwrap();
            for n in 1..=60u64 {
                let state = StateDBWrapper(
                    db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                        .unwrap()
                        .unwrap(),
                );
                state.update_block(block_info(n), diff(n)).unwrap();
            }
            let mut batch = db.prepare_write_batch().unwrap();
            db.write_code(&mut batch, word(901), Bytes::from(vec![1, 2, 3]))
                .unwrap();
            db.commit(batch).unwrap();
            // A trace of slot lookups with the version each one finds.
            let mut trace = Vec::new();
            for h in [1u64, 17, 33, 60] {
                let state = db
                    .db_at(BlockId::Number(BlockNumberOrTag::Number(h)))
                    .unwrap()
                    .unwrap();
                for (a, s) in (0..40u64).flat_map(|a| (0..5u64).map(move |s| (a, s))) {
                    let (address, slot) = (word(a), word(10_000 + a * 1000 + s));
                    let (_, version) = state.read_storage_and_height(address, slot).unwrap();
                    trace.extend_from_slice(&[5, 64]);
                    trace.extend_from_slice(address.as_slice());
                    trace.extend_from_slice(slot.as_slice());
                    trace.extend_from_slice(&h.to_le_bytes());
                    trace.extend_from_slice(&version.unwrap_or(u64::MAX).to_le_bytes());
                }
            }
            std::fs::write(base.join("trace"), trace).unwrap();
            read_all(&db, 60)
        };

        let report = DataBaseRef::convert_to_v4(&src, &dst, 3).unwrap();
        assert_eq!(report.head, 60);
        assert!(report.accounts > 300 && report.storage > 3000, "{report:?}");
        assert_eq!(report.above_head, 0);
        assert!(report.files >= 2);
        // Keys with few versions and keys read in several windows.
        let [whole, windows] = [0, 1].map(|i| KEY_PASSES[i].load(Ordering::Relaxed));
        assert!(whole > 20 && windows > 100, "{whole} {windows}");
        // Converting into it again is refused.
        assert!(DataBaseRef::convert_to_v4(&src, &dst, 3).is_err());

        let verify = verify_v4_against_source(&src, &dst, 300, 7).unwrap();
        assert_eq!(verify.mismatches, 0, "{verify:?}");
        assert!(verify.keys > 100 && verify.absent_keys > 100, "{verify:?}");
        // Slots inside the contract with many are among those compared.
        assert!(verify.inner_slots > 0, "{verify:?}");
        assert!(verify.blocks == 300 && verify.codes > 0, "{verify:?}");

        let [warm, run] = replay_get_trace(&dst, &base.join("trace"), 3, 64, 100).unwrap();
        assert_eq!((warm.gets, run.gets), (100, 700));
        assert_eq!(warm.mismatches + run.mismatches, 0);
        assert!(warm.reads > 0);

        let db = Arc::new(DataBaseRef::open(&dst, 64, false, false));
        assert!(db.is_schema_v4());
        assert_eq!(db.read_encoding_marker().unwrap(), Some(true));
        assert_eq!(read_all(&db, 60), want);
        for n in [1u64, 17, 60] {
            let info = db.read_block_info(block_info(n).header.hash).unwrap();
            assert_eq!(info.map(|info| info.header.number), Some(n));
        }
        assert_eq!(
            db.read_code(word(901)).unwrap(),
            Some(Bytes::from(vec![1, 2, 3]))
        );
        // It goes on from the source's head.
        let state = StateDBWrapper(
            db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                .unwrap()
                .unwrap(),
        );
        assert_eq!(
            state.last_committed_block().unwrap().unwrap().header.number,
            60
        );
        state.update_block(block_info(61), diff(61)).unwrap();
        drop(state);
        db.v4.as_ref().unwrap().flush().unwrap();
        drop(db);
        // The source went on the same way: still equal.
        {
            let db = Arc::new(DataBaseRef::open(&src, 64, false, false));
            let state = StateDBWrapper(
                db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                    .unwrap()
                    .unwrap(),
            );
            state.update_block(block_info(61), diff(61)).unwrap();
        }
        let verify = verify_v4_against_source(&src, &dst, 300, 9).unwrap();
        assert_eq!(verify.mismatches, 0, "{verify:?}");
        // A code and a block that only the source has are told.
        let other_code = |db: &Arc<DataBaseRef>| {
            let mut batch = db.prepare_write_batch().unwrap();
            db.write_code(&mut batch, H256::from([0xff; 32]), Bytes::from(vec![4, 5]))
                .unwrap();
            db.commit(batch).unwrap();
        };
        {
            let db = Arc::new(DataBaseRef::open(&src, 64, false, false));
            other_code(&db);
        }
        let verify = verify_v4_against_source(&src, &dst, 300, 9).unwrap();
        assert!(verify.mismatches > 0, "{verify:?}");
        // Unless the source went on: the code may be of a later block.
        {
            let db = Arc::new(DataBaseRef::open(&src, 64, false, false));
            append(&db, 62..=62);
        }
        let verify = verify_v4_against_source(&src, &dst, 300, 9).unwrap();
        assert_eq!(verify.mismatches, 0, "{verify:?}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
