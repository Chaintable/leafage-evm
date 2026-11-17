//! Offline conversion of a schema v3 archive to schema v4, and the check of
//! the result against its source (`docs/ArchiveSchemaV4.md`, "Conversion").

use super::super::block_index;
use super::super::udt::{
    self, encode_ts, hex, open_read_only, scan_options, write_schema_file, SCHEMA_FILE,
};
use super::super::{
    decode_archive_header, decode_cached_account, fmt_hms, shard_bounds, DataBaseRef,
    StorageTypeColumn,
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
/// Versions of one key held in memory by the conversion. The source sorts a
/// key's versions newest first and v4 wants them oldest first; a key with
/// more versions than this (an account that changes in every block has one
/// per block of the chain) is read again in block windows of this many.
#[cfg(not(test))]
const KEY_WINDOW: usize = 1 << 18;
#[cfg(test)]
const KEY_WINDOW: usize = 12;
/// Keys the conversion passed whole, and windows of the others.
#[cfg(test)]
static KEY_PASSES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Records and bytes written by [`DataBaseRef::convert_udt_to_v4`].
#[derive(Debug, Default, Clone)]
pub struct V4ConvertReport {
    /// Account versions.
    pub accounts: u64,
    /// Storage versions.
    pub storage: u64,
    /// Versions above the source's head, left behind.
    pub above_head: u64,
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

/// The key and block of the version an all-versions iterator is on.
fn version_key(raw: &[u8], k: usize) -> Result<(&[u8], u64), Error> {
    // The key, its 8-byte timestamp, then RocksDB's sequence trailer.
    if raw.len() < k + 8 {
        return Err(Error::UnSupported(format!(
            "unexpected schema v3 key of {} bytes",
            raw.len()
        )));
    }
    Ok((
        &raw[..k],
        u64::from_le_bytes(raw[k..k + 8].try_into().unwrap()),
    ))
}

/// A schema v3 value as a v4 one.
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

/// Reads a timestamped CF key by key: each key's versions, oldest first.
struct KeyVersions<'a> {
    db: &'a DB,
    iter: DBRawIteratorWithThreadMode<'a, DB>,
    storage: bool,
    /// Versions above this block are skipped and counted.
    head: u64,
    above_head: u64,
    /// The current key's versions; their `key` is the key.
    versions: Vec<Rec>,
    /// Of a key read in windows: one window, and the newest block of each
    /// window after the first, newest first.
    window: Vec<Rec>,
    cuts: Vec<u64>,
    bytes: u64,
}

impl<'a> KeyVersions<'a> {
    fn new(db: &'a DB, storage: bool, ro: ReadOptions, head: u64) -> Self {
        let col = if storage {
            StorageTypeColumn::AddressToStorage
        } else {
            StorageTypeColumn::AddressToAccount
        };
        let cf = db.cf_handle(col.to_str()).unwrap();
        Self {
            db,
            iter: db.raw_iterator_cf_opt(cf, all_versions(ro)),
            storage,
            head,
            above_head: 0,
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
        let mut key = [0u8; 64];
        let mut have = false;
        // Versions up to the head.
        let mut n = 0usize;
        while let Some(raw) = self.iter.key() {
            let (user, block) = version_key(raw, k)?;
            if have && user != &key[..k] {
                break;
            }
            key[..k].copy_from_slice(user);
            have = true;
            let value = self.iter.value().unwrap_or(&[]);
            self.bytes += (raw.len() + value.len()) as u64;
            if block > self.head {
                self.above_head += 1;
            } else {
                if n < KEY_WINDOW {
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
            self.iter.next();
        }
        self.iter.status()?;
        if !have {
            return Ok(false);
        }
        // The windows below the newest one, oldest first: each is read
        // with the timestamp bounds of its blocks.
        let mut passed = self.versions.len();
        for i in (0..self.cuts.len()).rev() {
            let lo = match self.cuts.get(i + 1) {
                Some(&older) => older + 1,
                None => 0,
            };
            let mut ro = ReadOptions::default();
            ro.set_verify_checksums(false);
            ro.set_total_order_seek(true);
            ro.set_readahead_size(2 << 20);
            ro.fill_cache(false);
            ro.set_timestamp(encode_ts(self.cuts[i]));
            ro.set_iter_start_ts(encode_ts(lo));
            let mut iter = self.db.raw_iterator_cf_opt(self.cf(), ro);
            iter.seek(&key[..k]);
            self.window.clear();
            while let Some(raw) = iter.key() {
                let (user, block) = version_key(raw, k)?;
                if user != &key[..k] {
                    break;
                }
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
        KEY_PASSES[usize::from(!self.cuts.is_empty())].fetch_add(1, Ordering::Relaxed);
        if passed != n {
            return Err(Error::UnSupported(format!(
                "key {} has {n} versions but {passed} in block windows",
                hex(&key[..k])
            )));
        }
        // Newest first in the source.
        self.versions.reverse();
        sink(&self.versions)?;
        Ok(true)
    }

    /// Load the versions of the key the iterator is on, at most `cap` of
    /// them (the newest), and move past the key. `false` at the end.
    fn next_key(&mut self, cap: usize) -> Result<bool, Error> {
        self.versions.clear();
        let k = key_len(self.storage);
        let mut key = [0u8; 64];
        let mut have = false;
        while let Some(raw) = self.iter.key() {
            let (user, block) = version_key(raw, k)?;
            if have && user != &key[..k] {
                break;
            }
            key[..k].copy_from_slice(user);
            have = true;
            let value = self.iter.value().unwrap_or(&[]);
            self.bytes += (raw.len() + value.len()) as u64;
            if block > self.head {
                self.above_head += 1;
            } else if self.versions.len() < cap {
                self.versions.push(Rec {
                    key,
                    block,
                    val: version_value(self.storage, value)?,
                });
            }
            self.iter.next();
        }
        self.iter.status()?;
        // Newest first in the source.
        self.versions.reverse();
        Ok(have)
    }
}

/// Train one dictionary for a kind from keys spread over the key space.
fn train_kind_dict(src: &DB, storage: bool, head: u64) -> Result<Vec<u8>, Error> {
    let mut ro = ReadOptions::default();
    ro.set_total_order_seek(true);
    ro.set_verify_checksums(false);
    ro.fill_cache(false);
    let mut keys = KeyVersions::new(src, storage, ro, head);
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
    /// Offline rewrite of a schema v3 archive into schema v4 at `dst_path`,
    /// which must not exist or be empty. The source is opened read-only.
    ///
    /// The versioned column families are read shard by shard and written as
    /// v4 base files holding every version up to the source's head; the
    /// result has no delta. The other column families are copied; block
    /// infos move to `BlockNumToBlockInfo` if `ARCHIVE_BLOCK_INDEX=1` asks
    /// for it and the source still has them by hash. The schema file is
    /// written last: an interrupted conversion cannot be opened as v4.
    pub fn convert_udt_to_v4(
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
        if !udt::is_udt_archive(src_path)? {
            return Err(Error::UnSupported(
                "source must be a schema v3 archive".into(),
            ));
        }
        let jobs = jobs.clamp(1, 64);
        let src = open_read_only(src_path, true)?;
        let head = {
            let meta = src
                .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
                .unwrap();
            let hash = src
                .get_cf(meta, [1u8])?
                .ok_or_else(|| Error::UnSupported("source archive is empty".into()))?;
            source_block_number(&src, &hash)?
        };
        if head > u32::MAX as u64 {
            return Err(Error::UnSupported(format!(
                "archive schema v4 holds block numbers up to 2^32, not {head}"
            )));
        }
        info!(target: "migrate", "v4 convert: source head is block {head}");

        let store_dir = dst_path.join(STORE_DIR);
        std::fs::create_dir_all(&store_dir)?;
        side::create(&store_dir)?;

        let start = std::time::Instant::now();
        let options = Options::maintenance(0);
        let mut dicts = Vec::new();
        for storage in [false, true] {
            let raw = train_kind_dict(&src, storage, head)?;
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
        // Storage versions, account versions, other records, above the head.
        let records = [const { AtomicU64::new(0) }; 4];
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

/// How far below its last block info the head of a source is looked for.
const HEAD_SEARCH_BLOCKS: usize = 1 << 20;

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
// Verification against the v3 source
// ---------------------------------------------------------------------------

/// Result of [`verify_v4_against_v3`].
#[derive(Debug, Default, Clone)]
pub struct V4VerifyReport {
    pub keys: u64,
    /// Storage keys among them that are not the first slot of their
    /// address.
    pub inner_slots: u64,
    pub reads: u64,
    pub absent_keys: u64,
    pub mismatches: u64,
}

/// Versions of one key kept for a check; hotter keys are checked over their
/// newest versions only.
const VERIFY_MAX_VERSIONS: usize = 4096;

/// Compare a v4 archive with the v3 archive it was converted from: for
/// `samples` random keys of each kind, read v4 at heights on, just below
/// and between the key's versions and above the newest, and compare value
/// and block with v3's; also a key next to each that v3 does not hold.
/// Heights go up to the last block the v4 archive's files hold.
pub fn verify_v4_against_v3(
    v3_path: &Path,
    v4_path: &Path,
    samples: u64,
    seed: u64,
) -> Result<V4VerifyReport, Error> {
    if !udt::is_udt_archive(v3_path)? || !udt::is_v4_archive(v4_path)? {
        return Err(Error::UnSupported(format!(
            "expected a schema v3 and a schema v4 archive ({SCHEMA_FILE})"
        )));
    }
    let v3 = open_read_only(v3_path, true)?;
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
        let mut keys = KeyVersions::new(&v3, storage, ro, head);
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
                    let first = version_key(raw, k)?.0.to_vec();
                    let mut inner = first.clone();
                    for b in &mut inner[32..] {
                        *b = next() as u8;
                    }
                    keys.iter.seek(&inner);
                    let landed = keys.iter.key().map(|raw| version_key(raw, k)).transpose()?;
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
                            "v4 verify mismatch: key {} at {h}: v3 {want:?} (next version {next_version}), v4 {read:?}",
                            hex(&key[..k]));
                    }
                }
            }
            // A key next to it that v3 does not hold.
            let mut absent = key;
            absent[k - 1] ^= 0x5a;
            keys.iter.seek(&absent[..k]);
            let held = match keys.iter.key() {
                Some(raw) => version_key(raw, k)?.0 == &absent[..k],
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
                    warn!(target: "migrate", "v4 verify: key {} absent in v3 but found", hex(&absent[..k]));
                }
            }
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
        let read_all = |db: &Arc<DataBaseRef>, head: u64| {
            let mut out = Vec::new();
            for h in (1..=head).step_by(3).chain([head]) {
                let state = db
                    .db_at(BlockId::Number(BlockNumberOrTag::Number(h)))
                    .unwrap()
                    .unwrap();
                for a in 0..42u64 {
                    out.push(format!("{:?}", state.read_account(word(a)).unwrap()));
                    for s in (0..if a == 1 { 302 } else { 4 }).step_by(if a == 1 { 17 } else { 1 })
                    {
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
        };
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

        let report = DataBaseRef::convert_udt_to_v4(&src, &dst, 3).unwrap();
        assert_eq!(report.head, 60);
        assert!(report.accounts > 300 && report.storage > 3000, "{report:?}");
        assert_eq!(report.above_head, 0);
        assert!(report.files >= 2);
        // Keys with few versions and keys read in several windows.
        let [whole, windows] = [0, 1].map(|i| KEY_PASSES[i].load(Ordering::Relaxed));
        assert!(whole > 20 && windows > 100, "{whole} {windows}");
        // Converting into it again is refused.
        assert!(DataBaseRef::convert_udt_to_v4(&src, &dst, 3).is_err());

        let verify = verify_v4_against_v3(&src, &dst, 300, 7).unwrap();
        assert_eq!(verify.mismatches, 0, "{verify:?}");
        assert!(verify.keys > 100 && verify.absent_keys > 100, "{verify:?}");
        // Slots inside the contract with many are among those compared.
        assert!(verify.inner_slots > 0, "{verify:?}");

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
        let verify = verify_v4_against_v3(&src, &dst, 300, 9).unwrap();
        assert_eq!(verify.mismatches, 0, "{verify:?}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
