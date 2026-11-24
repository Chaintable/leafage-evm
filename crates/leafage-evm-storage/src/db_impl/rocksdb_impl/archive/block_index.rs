//! Block infos keyed by number (CF `7`), found by hash through an in-memory
//! index of the canonical blocks.
//!
//! Requests name historical blocks by hash. `BlockHashToBlockInfo` (CF `2`) is
//! keyed by hash, so consecutive blocks lie in unrelated data blocks and a
//! replay over a range of blocks reads one data block per block. CF `7` holds
//! the same records keyed by number in 16KB blocks, so consecutive blocks
//! share their data blocks. Each record starts with its block hash, so a
//! lookup that reaches another block through a shared hash prefix tries the
//! next candidate.
//!
//! An archive keeps its block infos in one of the two CFs. With
//! `ARCHIVE_BLOCK_INDEX=1` a new database is created with CF `7`, and opening
//! one with CF `2` moves them before the open returns: the canonical records
//! of CF `2` are copied into CF `7`, then CF `2` is dropped. A move that is
//! interrupted starts over at the next open.
//!
//! The index splits the blocks into 256 shards by the hash's first byte.
//! Within a shard, the next bits of the hash pick a bucket, about 8 blocks
//! each, and the 8 bits after them are kept as a fingerprint next to the
//! block number: 5 bytes per block and 4 per bucket (~0.7GB for bsc). Block
//! numbers must fit in a `u32`.
//!
//! The index is saved to [`FILE`] in the database directory after each merge
//! of new blocks. An open loads it and indexes the blocks of
//! `BlockNumToBlockHash` (CF `3`) past its last one; it rebuilds the index
//! from CF `3` instead when the file is missing, unreadable, ends at a block
//! CF `3` no longer has (a rewind), or has too few buckets for the chain.

use super::*;
use crate::db_impl::rocksdb_impl::sst::{ingest_external_file_options, sst_writer_options};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::Instant;

/// The move sorts CF `2` records by number in chunks of this size.
const CHUNK_BYTES: usize = if cfg!(test) { 4096 } else { 1 << 30 };

/// New blocks are merged into the shards in batches of this size.
const MERGE_AT: usize = if cfg!(test) { 4 } else { 1 << 20 };

const SHARDS: usize = 256;

/// The saved index, in the database directory.
pub(super) const FILE: &str = "LEAFAGE_BLOCK_INDEX";

const MAGIC: &[u8; 8] = b"LFBIDX01";

/// Threads that scan CF `3` when the index is rebuilt.
const SCAN_THREADS: usize = 8;

// The file holds the arrays in memory order.
const _: () = assert!(cfg!(target_endian = "little"));

pub(super) fn enabled() -> bool {
    matches!(
        env::var("ARCHIVE_BLOCK_INDEX").as_deref(),
        Ok("1" | "true" | "TRUE")
    )
}

#[inline]
pub(super) fn key(number: u64) -> [u8; 8] {
    number.to_be_bytes()
}

/// A record of CF `7`: the block hash, then the CF `2` value.
pub(super) fn record(hash: H256, info: &[u8]) -> Vec<u8> {
    let mut record = Vec::with_capacity(32 + info.len());
    record.extend_from_slice(hash.as_slice());
    record.extend_from_slice(info);
    record
}

/// A block to index: its hash and number.
pub(super) fn entry(hash: &H256, number: u64) -> Result<(H256, u32), Error> {
    let number = u32::try_from(number).map_err(|_| {
        Error::UnSupported(format!("block {number} is past the archive block index"))
    })?;
    Ok((*hash, number))
}

/// The hash's first 8 bytes: the shard, then the bucket and fingerprint.
#[inline]
fn prefix(hash: &H256) -> u64 {
    u64::from_be_bytes(hash[..8].try_into().unwrap())
}

#[inline]
fn shard_of(prefix: u64) -> usize {
    (prefix >> 56) as usize
}

/// `bucket << 8 | fingerprint` of a hash prefix, with `k` bucket bits.
#[inline]
fn code(prefix: u64, k: u32) -> u32 {
    ((prefix >> 24) as u32) >> (24 - k)
}

/// The fewest bucket bits: a database synced from scratch by one process
/// keeps at most 16 blocks per bucket on average until 2^28 blocks, as
/// merges keep the buckets of the open.
const MIN_BUCKET_BITS: u32 = if cfg!(test) { 0 } else { 16 };

/// Bucket bits per shard for `blocks` blocks: about 8 blocks per bucket on
/// average.
fn bucket_bits(blocks: u64) -> u32 {
    let log2 = u64::BITS - blocks.saturating_sub(1).leading_zeros();
    log2.saturating_sub(11).clamp(MIN_BUCKET_BITS, 24)
}

/// One shard: entries sorted by bucket, fingerprint and number.
struct Shard {
    /// Bucket bits.
    k: u32,
    /// Entries `offsets[b]..offsets[b + 1]` are in bucket `b`.
    offsets: Vec<u32>,
    fingerprints: Vec<u8>,
    numbers: Vec<u32>,
}

impl Shard {
    /// From `(code, number)` entries, sorted and unique.
    fn from_sorted(k: u32, entries: &[(u32, u32)]) -> Self {
        let buckets = 1usize << k;
        let mut offsets = Vec::with_capacity(buckets + 1);
        let mut fingerprints = Vec::with_capacity(entries.len());
        let mut numbers = Vec::with_capacity(entries.len());
        offsets.push(0);
        for &(code, number) in entries {
            let bucket = (code >> 8) as usize;
            while offsets.len() <= bucket {
                offsets.push(numbers.len() as u32);
            }
            fingerprints.push(code as u8);
            numbers.push(number);
        }
        offsets.resize(buckets + 1, numbers.len() as u32);
        Self {
            k,
            offsets,
            fingerprints,
            numbers,
        }
    }

    fn entries(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.offsets.windows(2).enumerate().flat_map(move |(b, w)| {
            (w[0] as usize..w[1] as usize).map(move |i| {
                (
                    (b as u32) << 8 | u32::from(self.fingerprints[i]),
                    self.numbers[i],
                )
            })
        })
    }

    /// This shard with `new` entries, sorted and unique, merged in.
    fn merged(&self, new: &[(u32, u32)]) -> Self {
        let mut all = Vec::with_capacity(self.numbers.len() + new.len());
        let mut new = new.iter().copied().peekable();
        for e in self.entries() {
            while let Some(n) = new.next_if(|&n| n <= e) {
                if n < e {
                    all.push(n);
                }
            }
            all.push(e);
        }
        all.extend(new);
        Self::from_sorted(self.k, &all)
    }

    fn candidates(&self, prefix: u64, out: &mut Vec<u64>) {
        let code = code(prefix, self.k);
        let bucket = (code >> 8) as usize;
        let range = self.offsets[bucket] as usize..self.offsets[bucket + 1] as usize;
        for (i, &fp) in self.fingerprints[range.clone()].iter().enumerate() {
            if fp == code as u8 {
                out.push(u64::from(self.numbers[range.start + i]));
            }
        }
    }
}

/// Blocks indexed since the last merge.
#[derive(Default)]
struct Recent {
    /// The hash prefix and number of each block.
    new: BTreeSet<(u64, u32)>,
    /// The entries a merge is moving into the shards.
    merging: Arc<BTreeSet<(u64, u32)>>,
    /// The block last noted in `new`.
    last: Option<(u32, H256)>,
}

/// Where the hashes of the canonical blocks are: CF `3`, or the block files
/// of a schema v4 archive.
pub(super) trait BlockHashes: Sync {
    /// The newest block.
    fn last(&self) -> Result<Option<(u64, H256)>, Error>;
    /// The hash of block `number`.
    fn hash(&self, number: u64) -> Result<Option<H256>, Error>;
    /// The blocks in `[from, to)`, in order. Of each hash only the first 8
    /// bytes need to be right: the index is built from those.
    fn scan(
        &self,
        from: u64,
        to: u64,
        f: &mut dyn FnMut(u64, H256) -> Result<(), Error>,
    ) -> Result<(), Error>;
}

impl BlockHashes for DB {
    fn last(&self) -> Result<Option<(u64, H256)>, Error> {
        last_block(self)
    }

    fn hash(&self, number: u64) -> Result<Option<H256>, Error> {
        let cf = self
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .unwrap();
        Ok(self
            .get_pinned_cf(cf, encode_block_num(number))?
            .map(|hash| H256::from_slice(&hash)))
    }

    fn scan(
        &self,
        from: u64,
        to: u64,
        f: &mut dyn FnMut(u64, H256) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let cf = self
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .unwrap();
        let mut opts = scan_options();
        opts.set_iterate_lower_bound(encode_block_num(from));
        if to != u64::MAX {
            opts.set_iterate_upper_bound(encode_block_num(to));
        }
        for item in self.iterator_cf_opt(cf, opts, IteratorMode::Start) {
            let (number, hash) = item?;
            f(block_number(&number), H256::from_slice(&hash))?;
        }
        Ok(())
    }
}

impl BlockHashes for super::v4::Store {
    fn last(&self) -> Result<Option<(u64, H256)>, Error> {
        // The head, or the last block below it that has a hash: a block
        // written with its versions only has none.
        let Some(head) = self.head() else {
            return Ok(None);
        };
        let mut to = head + 1;
        while to > 0 {
            let from = to.saturating_sub(4096);
            let mut last = None;
            self.scan_block_prefixes(from, to, &mut |number, _| {
                last = Some(number);
                Ok(())
            })?;
            if let Some(number) = last {
                if let Some(hash) = self.block_hash(number)? {
                    return Ok(Some((number, H256::from(hash))));
                }
            }
            to = from;
        }
        Ok(None)
    }

    fn hash(&self, number: u64) -> Result<Option<H256>, Error> {
        Ok(self.block_hash(number)?.map(H256::from))
    }

    fn scan(
        &self,
        from: u64,
        to: u64,
        f: &mut dyn FnMut(u64, H256) -> Result<(), Error>,
    ) -> Result<(), Error> {
        // The block files keep the first 8 bytes of each hash.
        self.scan_block_prefixes(from, to, &mut |number, prefix| {
            let mut hash = H256::ZERO;
            hash.0[..8].copy_from_slice(&prefix.to_be_bytes());
            f(number, hash)
        })
    }
}

pub(super) struct BlockIndex {
    shards: Vec<RwLock<Arc<Shard>>>,
    recent: RwLock<Recent>,
    /// The block last merged into the shards; every block CF `3` had when
    /// it was committed is in them too. None once a block below it, or
    /// another block at it, is written, until the next merge.
    last: Mutex<Option<(u32, H256)>>,
    /// Held by a merge.
    merge: Mutex<()>,
    /// Where merges save the index.
    file: Option<PathBuf>,
    /// The save running in the background.
    saving: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for BlockIndex {
    fn drop(&mut self) {
        // Before the database, and its lock, when an open fails.
        self.wait_saved();
    }
}

impl Debug for BlockIndex {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockIndex")
            .field("k", &self.k())
            .field("last", &*self.last.lock().unwrap())
            .finish()
    }
}

impl BlockIndex {
    fn new(shards: Vec<Shard>, last: Option<(u32, H256)>) -> Self {
        Self {
            shards: shards
                .into_iter()
                .map(|s| RwLock::new(Arc::new(s)))
                .collect(),
            recent: RwLock::default(),
            last: Mutex::new(last),
            merge: Mutex::new(()),
            file: None,
            saving: Mutex::new(None),
        }
    }

    /// The index of the archive in `dir`: loaded from its file and completed
    /// from CF `3`, or rebuilt from CF `3` and saved.
    pub(super) fn open(db: &(impl BlockHashes + ?Sized), dir: &Path) -> Result<Self, Error> {
        let start = Instant::now();
        let file = dir.join(FILE);
        let blocks = db.last()?.map_or(0, |(number, _)| number + 1);
        let loaded = match load(&file) {
            Ok(Some((k, shards, last))) => {
                if db.hash(u64::from(last.0))? != Some(last.1) {
                    warn!(target: "rocksdb", last = last.0, "archive block index file ends at a block CF 3 no longer has, rebuilding");
                    None
                } else if k + 1 < bucket_bits(blocks) {
                    warn!(target: "rocksdb", k, blocks, "archive block index file has too few buckets, rebuilding");
                    None
                } else {
                    Some(Self::new(shards, Some(last)))
                }
            }
            Ok(None) => None,
            Err(e) => {
                warn!(target: "rocksdb", error = %e, "archive block index file is unreadable, rebuilding");
                None
            }
        };
        match loaded {
            Some(mut index) => {
                index.file = Some(file);
                let from = u64::from(index.last.lock().unwrap().unwrap().0) + 1;
                let tail = index.index_from(db, from)?;
                info!(target: "rocksdb", blocks, tail, secs = start.elapsed().as_secs(),
                    "archive block index loaded");
                Ok(index)
            }
            None => Self::rebuild(db, dir),
        }
    }

    /// The index of the archive in `dir`, built from CF `3` and saved. The
    /// move from CF 2 uses it: a file left by a failed move is not kept up
    /// to date while the archive runs on CF 2.
    pub(super) fn rebuild(db: &(impl BlockHashes + ?Sized), dir: &Path) -> Result<Self, Error> {
        let start = Instant::now();
        let file = dir.join(FILE);
        let mut index = Self::build(db)?;
        index.save_to(&file)?;
        index.file = Some(file);
        info!(target: "rocksdb", blocks = index.len(), secs = start.elapsed().as_secs(),
            "archive block index built");
        Ok(index)
    }

    /// Index every block of CF `3`, scanning ranges of block numbers in
    /// parallel.
    pub(super) fn build(db: &(impl BlockHashes + ?Sized)) -> Result<Self, Error> {
        let last = db.last()?;
        let blocks = last.map_or(0, |(number, _)| number + 1);
        let k = bucket_bits(blocks);
        let per = blocks.div_ceil(SCAN_THREADS as u64).max(1);
        // Entries by scanning thread, then by shard.
        let mut scanned = std::thread::scope(|s| {
            let scans: Vec<_> = (0..SCAN_THREADS as u64)
                .map(|i| {
                    s.spawn(move || -> Result<Vec<Vec<(u32, u32)>>, Error> {
                        let mut shards = vec![Vec::new(); SHARDS];
                        db.scan(i * per, (i + 1) * per, &mut |number, hash| {
                            let (hash, number) = entry(&hash, number)?;
                            let p = prefix(&hash);
                            shards[shard_of(p)].push((code(p, k), number));
                            Ok(())
                        })?;
                        Ok(shards)
                    })
                })
                .collect();
            scans
                .into_iter()
                .map(|scan| scan.join().unwrap())
                .collect::<Result<Vec<_>, Error>>()
        })?;
        // Each thread sorts a range of shards.
        let mut by_shard: Vec<Vec<Vec<(u32, u32)>>> = (0..SHARDS)
            .map(|s| {
                scanned
                    .iter_mut()
                    .map(|t| std::mem::take(&mut t[s]))
                    .collect()
            })
            .collect();
        let shards: Vec<Shard> = std::thread::scope(|s| {
            let sorts: Vec<_> = by_shard
                .chunks_mut(SHARDS / SCAN_THREADS)
                .map(|chunk| {
                    s.spawn(move || {
                        chunk
                            .iter_mut()
                            .map(|parts| {
                                let mut entries = parts.concat();
                                parts.clear();
                                entries.sort_unstable();
                                Shard::from_sorted(k, &entries)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            sorts
                .into_iter()
                .flat_map(|sort| sort.join().unwrap())
                .collect()
        });
        let last = last
            .map(|(number, hash)| entry(&hash, number).map(|(hash, number)| (number, hash)))
            .transpose()?;
        Ok(Self::new(shards, last))
    }

    /// Index the blocks of CF `3` from `from` on; returns how many.
    fn index_from(&self, db: &(impl BlockHashes + ?Sized), from: u64) -> Result<usize, Error> {
        let mut blocks = 0;
        let mut batch = Vec::new();
        // The last block of a batch is remembered with its whole hash.
        let whole = |batch: &mut Vec<(H256, u32)>| -> Result<(), Error> {
            if let Some(last) = batch.last_mut() {
                last.0 = db.hash(u64::from(last.1))?.ok_or_else(|| {
                    Error::UnSupported(format!("block {} is gone while it is indexed", last.1))
                })?;
            }
            Ok(())
        };
        db.scan(from, u64::MAX, &mut |number, hash| {
            batch.push(entry(&hash, number)?);
            if batch.len() == MERGE_AT {
                blocks += batch.len();
                whole(&mut batch)?;
                self.note(&batch)?;
                batch.clear();
                self.merge(false);
            }
            Ok(())
        })?;
        blocks += batch.len();
        whole(&mut batch)?;
        self.note(&batch)?;
        if blocks >= MERGE_AT {
            self.merge(true);
        }
        Ok(blocks)
    }

    /// The numbers that may hold the block `hash`.
    pub(super) fn candidates(&self, hash: &H256) -> Vec<u64> {
        let p = prefix(hash);
        let mut numbers = Vec::new();
        let recent = self.recent.read().unwrap();
        for set in [&recent.new, &*recent.merging] {
            numbers.extend(
                set.range((p, 0)..=(p, u32::MAX))
                    .map(|&(_, n)| u64::from(n)),
            );
        }
        let shard = self.shards[shard_of(p)].read().unwrap().clone();
        drop(recent);
        let from = numbers.len();
        shard.candidates(p, &mut numbers);
        // A block a merge has moved but not yet dropped from `merging`.
        if from > 0 {
            numbers.sort_unstable();
            numbers.dedup();
        }
        numbers
    }

    /// Whether block `number` may be `hash`.
    fn contains(&self, hash: &H256, number: u64) -> bool {
        self.candidates(hash).contains(&number)
    }

    /// Blocks indexed.
    fn len(&self) -> usize {
        let recent = self.recent.read().unwrap();
        let shards: usize = self
            .shards
            .iter()
            .map(|s| s.read().unwrap().numbers.len())
            .sum();
        shards + recent.new.len()
    }

    /// Index blocks about to be committed. The record carries its hash, so
    /// an entry ahead of its write only misses.
    pub(super) fn note(&self, blocks: &[(H256, u32)]) -> Result<(), Error> {
        let Some(lowest) = blocks.iter().map(|&(_, number)| number).min() else {
            return Ok(());
        };
        // No merge saves a snapshot without these blocks once the check
        // below has passed.
        let _merge = self.merge.lock().unwrap();
        // On its first commit after an open the state tree writes the head
        // it opened on again, unchanged; that keeps the file.
        if self.last.lock().unwrap().is_some_and(|(last, last_hash)| {
            blocks
                .iter()
                .any(|&(hash, number)| number < last || (number == last && hash != last_hash))
        }) {
            // Blocks the file covers are rewritten (after a head-only
            // rewind), while CF 3 may still hold its last one: drop it before
            // the write. `last` stays until the file is gone, so a failed
            // drop is retried.
            self.wait_saved();
            if let Some(file) = &self.file {
                match std::fs::remove_file(file) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                    _ => {}
                }
                if let Some(dir) = file.parent() {
                    File::open(dir)?.sync_all()?;
                }
                warn!(target: "rocksdb", block = lowest, "archive block rewritten below the saved index, file dropped");
            }
            *self.last.lock().unwrap() = None;
        }
        let mut recent = self.recent.write().unwrap();
        for &(hash, number) in blocks {
            recent.new.insert((prefix(&hash), number));
            recent.last = Some((number, hash));
        }
        Ok(())
    }

    /// After a commit: merge the new blocks into the shards once there are
    /// enough of them, and save the index in the background.
    pub(super) fn committed(&self) {
        if self.recent.read().unwrap().new.len() >= MERGE_AT {
            self.merge(true);
        }
    }

    /// Bucket bits of the shards.
    fn k(&self) -> u32 {
        self.shards[0].read().unwrap().k
    }

    /// Merge `recent.new` into the shards, one shard at a time. The buckets
    /// stay those of the open, whose file check adds more as the chain grows.
    fn merge(&self, save_after: bool) {
        let _merge = self.merge.lock().unwrap();
        // The last save holds the shards it saved.
        self.wait_saved();
        let (batch, last) = {
            let mut recent = self.recent.write().unwrap();
            let batch = Arc::new(std::mem::take(&mut recent.new));
            recent.merging = batch.clone();
            (batch, recent.last.take())
        };
        let mut entries = batch.iter().peekable();
        for (s, shard) in self.shards.iter().enumerate() {
            let old = shard.read().unwrap().clone();
            let mut new: Vec<(u32, u32)> =
                std::iter::from_fn(|| entries.next_if(|(p, _)| shard_of(*p) == s))
                    .map(|&(p, n)| (code(p, old.k), n))
                    .collect();
            if new.is_empty() {
                continue;
            }
            new.sort_unstable();
            new.dedup();
            let merged = Arc::new(old.merged(&new));
            drop(old);
            *shard.write().unwrap() = merged;
        }
        self.recent.write().unwrap().merging = Arc::default();
        let last = {
            let mut shard_last = self.last.lock().unwrap();
            if last.is_some() {
                *shard_last = last;
            }
            *shard_last
        };
        let Some(file) = self.file.clone().filter(|_| save_after) else {
            return;
        };
        // Taken under the merge lock: the shards hold every block up to `last`.
        let shards = self.snapshot();
        *self.saving.lock().unwrap() = Some(std::thread::spawn(move || {
            if let Err(e) = save(&file, &shards, last.as_ref()) {
                warn!(target: "rocksdb", error = %e, "saving the archive block index failed");
            }
        }));
    }

    /// Wait for the save running in the background, if any.
    pub(super) fn wait_saved(&self) {
        if let Some(saving) = self.saving.lock().unwrap().take() {
            let _ = saving.join();
        }
    }

    fn snapshot(&self) -> Vec<Arc<Shard>> {
        self.shards
            .iter()
            .map(|s| s.read().unwrap().clone())
            .collect()
    }

    /// Save the index to `file`, for an archive that has its blocks.
    pub(super) fn save_to(&self, file: &Path) -> Result<(), Error> {
        save(file, &self.snapshot(), self.last.lock().unwrap().as_ref())
    }
}

/// The newest block of CF `3`.
fn last_block(db: &DB) -> Result<Option<(u64, H256)>, Error> {
    let cf = db
        .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
        .unwrap();
    db.iterator_cf(cf, IteratorMode::End)
        .next()
        .map(|item| {
            let (number, hash) = item?;
            Ok((block_number(&number), H256::from_slice(&hash)))
        })
        .transpose()
}

/// The file: a 64-byte header (magic, bucket bits, block count, last block
/// number and hash), each shard's offsets, fingerprints and numbers, then a
/// CRC32 of all of it.
fn save(file: &Path, shards: &[Arc<Shard>], last: Option<&(u32, H256)>) -> Result<(), Error> {
    let Some(&(last, hash)) = last else {
        return Ok(());
    };
    let start = Instant::now();
    let tmp = file.with_extension("tmp");
    let mut out = Crc::new(BufWriter::with_capacity(8 << 20, File::create(&tmp)?));
    let blocks: usize = shards.iter().map(|s| s.numbers.len()).sum();
    out.write_all(MAGIC)?;
    out.write_all(&shards[0].k.to_le_bytes())?;
    out.write_all(&[0; 4])?;
    out.write_all(&(blocks as u64).to_le_bytes())?;
    out.write_all(&u64::from(last).to_le_bytes())?;
    out.write_all(hash.as_slice())?;
    for shard in shards {
        out.write_all(as_bytes(&shard.offsets))?;
        out.write_all(&shard.fingerprints)?;
        out.write_all(as_bytes(&shard.numbers))?;
    }
    let crc = out.crc.finalize();
    let mut out = out.inner;
    out.write_all(&crc.to_le_bytes())?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    std::fs::rename(&tmp, file)?;
    if let Some(dir) = file.parent() {
        File::open(dir)?.sync_all()?;
    }
    info!(target: "rocksdb", blocks, last, secs = start.elapsed().as_secs(), "archive block index saved");
    Ok(())
}

type Loaded = (u32, Vec<Shard>, (u32, H256));

/// The index saved in `file`, if there is one.
fn load(file: &Path) -> Result<Option<Loaded>, Error> {
    let f = match File::open(file) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let bad = |what: &str| Error::UnSupported(format!("{}: {what}", file.display()));
    let mut input = Crc::new(BufReader::with_capacity(8 << 20, f));
    let mut header = [0u8; 64];
    input.read_exact(&mut header)?;
    if &header[..8] != MAGIC {
        return Err(bad("not an archive block index"));
    }
    let k = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if k > 24 {
        return Err(bad("bad bucket bits"));
    }
    let blocks = u64::from_le_bytes(header[16..24].try_into().unwrap()) as usize;
    let last = u32::try_from(u64::from_le_bytes(header[24..32].try_into().unwrap()))
        .map_err(|_| bad("bad last block"))?;
    let hash = H256::from_slice(&header[32..64]);
    let mut shards = Vec::with_capacity(SHARDS);
    let mut read = 0;
    for _ in 0..SHARDS {
        let mut offsets = vec![0u32; (1usize << k) + 1];
        input.read_exact(as_bytes_mut(&mut offsets))?;
        let len = *offsets.last().unwrap() as usize;
        read += len;
        if read > blocks {
            return Err(bad("more blocks than its header says"));
        }
        let mut fingerprints = vec![0u8; len];
        input.read_exact(&mut fingerprints)?;
        let mut numbers = vec![0u32; len];
        input.read_exact(as_bytes_mut(&mut numbers))?;
        shards.push(Shard {
            k,
            offsets,
            fingerprints,
            numbers,
        });
    }
    let crc = input.crc.finalize();
    let mut stored = [0u8; 4];
    input.inner.read_exact(&mut stored)?;
    if read != blocks || crc != u32::from_le_bytes(stored) || input.inner.read(&mut [0])? != 0 {
        return Err(bad("checksum mismatch"));
    }
    Ok(Some((k, shards, (last, hash))))
}

/// A reader or writer that checksums what goes through it.
struct Crc<T> {
    inner: T,
    crc: crc32fast::Hasher,
}

impl<T> Crc<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            crc: crc32fast::Hasher::new(),
        }
    }
}

impl<W: Write> Write for Crc<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<R: Read> Read for Crc<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.crc.update(&buf[..n]);
        Ok(n)
    }
}

fn as_bytes(v: &[u32]) -> &[u8] {
    // SAFETY: any u32 is 4 initialized bytes, and u8 has no alignment.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), std::mem::size_of_val(v)) }
}

fn as_bytes_mut(v: &mut [u32]) -> &mut [u8] {
    // SAFETY: as above, and any 4 bytes are a valid u32.
    unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr().cast(), std::mem::size_of_val(v)) }
}

/// Copy the records of the canonical blocks, those `index` holds, from CF `2`
/// of `src` into the empty CF `7` of `dst`, using `tmp` for SST files; returns
/// the records copied. CF `2` is in hash order, so the records are sorted by
/// number in memory chunks, written to SST files, ingested together and merged
/// by one compaction instead of a compaction per chunk. Fails unless every
/// canonical block was copied.
pub(super) fn copy_block_infos(
    src: &DB,
    dst: &DB,
    tmp: &Path,
    index: &BlockIndex,
) -> Result<usize, Error> {
    let start = Instant::now();
    let canonical = index.len();
    let _ = std::fs::remove_dir_all(tmp);
    std::fs::create_dir_all(tmp)?;
    let opts = sst_writer_options();
    let headers = src
        .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
        .unwrap();
    let mut chunk = Vec::new();
    let mut chunk_bytes = 0;
    let mut files = Vec::new();
    let mut records = 0;
    for item in src.iterator_cf_opt(headers, scan_options(), IteratorMode::Start) {
        let (hash, info) = item?;
        let hash = H256::from_slice(&hash);
        let number = decode_archive_header(&mut &info[..])?.number;
        // Skips fork blocks, which share their number with a canonical one.
        if !index.contains(&hash, number) {
            continue;
        }
        let record = record(hash, &info);
        chunk_bytes += record.len();
        chunk.push((number, record));
        if chunk_bytes >= CHUNK_BYTES {
            records += write_chunk(&opts, tmp, files.len(), &mut chunk, &mut files)?;
            chunk_bytes = 0;
            if files.len() % 16 == 0 {
                info!(target: "rocksdb", records, secs = start.elapsed().as_secs(),
                    "moving block infos: copying");
            }
        }
    }
    if !chunk.is_empty() {
        records += write_chunk(&opts, tmp, files.len(), &mut chunk, &mut files)?;
    }
    if records != canonical {
        return Err(Error::UnSupported(format!(
            "moving block infos: copied {records} of {canonical} canonical blocks"
        )));
    }
    info!(target: "rocksdb", records, files = files.len(), secs = start.elapsed().as_secs(),
        "moving block infos: merging");
    if !files.is_empty() {
        let cf = dst
            .cf_handle(StorageTypeColumn::BlockNumToBlockInfo.to_str())
            .unwrap();
        dst.ingest_external_file_cf_opts(cf, &ingest_external_file_options(), files)?;
        let mut compact = CompactOptions::default();
        // The ingested files have the SST writer's layout, not the CF's
        // filters and index, and may land in the last level. Files this
        // compaction writes there are not rewritten again.
        compact.set_bottommost_level_compaction(BottommostLevelCompaction::ForceOptimized);
        dst.compact_range_cf_opt(cf, None::<&[u8]>, None::<&[u8]>, &compact);
    }
    std::fs::remove_dir_all(tmp)?;
    info!(target: "rocksdb", records, secs = start.elapsed().as_secs(), "moving block infos: copied");
    Ok(records)
}

fn block_number(key: &[u8]) -> u64 {
    U256::from_be_slice(key).try_into().unwrap()
}

fn scan_options() -> ReadOptions {
    let mut opts = ReadOptions::default();
    opts.set_verify_checksums(false);
    opts.fill_cache(false);
    opts.set_readahead_size(8 << 20);
    opts
}

/// Write a chunk sorted by number to a new SST file; returns its records.
fn write_chunk(
    opts: &Options,
    dir: &Path,
    n: usize,
    chunk: &mut Vec<(u64, Vec<u8>)>,
    files: &mut Vec<PathBuf>,
) -> Result<usize, Error> {
    chunk.sort_unstable_by_key(|(number, _)| *number);
    // Two blocks of one number pass the index only when their hashes share
    // a prefix; either record may stay, a lookup checks the hash.
    chunk.dedup_by_key(|(number, _)| *number);
    let path = dir.join(format!("{n:06}.sst"));
    let mut writer = SstFileWriter::create(opts);
    writer.open(&path)?;
    let records = chunk.len();
    for (number, record) in chunk.drain(..) {
        writer.put(key(number), record)?;
    }
    writer.finish()?;
    files.push(path);
    Ok(records)
}

impl DataBaseRef {
    /// The block info of `hash` from CF `7`.
    pub(super) fn read_block_info_by_number(
        &self,
        index: &BlockIndex,
        hash: H256,
    ) -> Result<Option<BlockInfo>, Error> {
        if let Some(store) = &self.v4 {
            for number in index.candidates(&hash) {
                let start = Instant::now();
                let block = store.block(number)?;
                STORAGE_METRICS
                    .read_block_by_number_latency
                    .record(start.elapsed().as_secs_f64());
                if let Some((_, info)) = block.filter(|block| block.0 == hash.0) {
                    return decode_block_info(hash, &info).map(Some);
                }
            }
            return Ok(None);
        }
        let cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockNumToBlockInfo.to_str())
            .unwrap();
        for number in index.candidates(&hash) {
            let start = Instant::now();
            let record = self
                .db
                .get_pinned_cf_opt(cf, key(number), &rocksdb_read_options())?;
            STORAGE_METRICS
                .read_block_by_number_latency
                .record(start.elapsed().as_secs_f64());
            if let Some(record) = record.filter(|r| r.starts_with(hash.as_slice())) {
                return decode_block_info(hash, &record[32..]).map(Some);
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::StateDBWrite;

    fn hash(number: u64, fork: bool) -> H256 {
        let mut h = [0u8; 32];
        h[0] = fork as u8;
        h[24..].copy_from_slice(&number.to_be_bytes());
        // Spread the blocks over CF 2, which is in hash order.
        h[1..9].copy_from_slice(&number.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
        H256::from(h)
    }

    fn info(number: u64, fork: bool) -> BlockInfo {
        let raw = RawHeader {
            number,
            extra_data: vec![7u8; 600].into(),
            ..Default::default()
        };
        BlockInfo::new(Block {
            header: Header {
                hash: hash(number, fork),
                inner: raw,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    /// Commit canonical blocks `numbers`, or fork block infos alone.
    fn commit(db: &Arc<DataBaseRef>, numbers: impl IntoIterator<Item = u64>, fork: bool) {
        let mut batch = db.prepare_write_batch().unwrap();
        for number in numbers {
            let info = info(number, fork);
            if !fork {
                db.write_block_hash(&mut batch, number, info.header.hash)
                    .unwrap();
                db.write_latest_block_hash(&mut batch, info.header.hash)
                    .unwrap();
            }
            db.write_block_info(&mut batch, info).unwrap();
        }
        db.commit(batch).unwrap();
    }

    fn columns(dir: &Path) -> Vec<StorageTypeColumn> {
        existing_columns(dir).unwrap()
    }

    fn records(db: &DataBaseRef) -> Vec<u64> {
        let cf = db
            .db
            .cf_handle(StorageTypeColumn::BlockNumToBlockInfo.to_str())
            .unwrap();
        db.db
            .iterator_cf(cf, IteratorMode::Start)
            .map(|item| u64::from_be_bytes(item.unwrap().0[..].try_into().unwrap()))
            .collect()
    }

    fn assert_reads(db: &DataBaseRef, numbers: impl IntoIterator<Item = u64>) {
        for number in numbers {
            let got = db.read_block_info(hash(number, false)).unwrap();
            assert_eq!(got, Some(info(number, false)), "block {number}");
        }
    }

    fn open(dir: &Path, by_number: bool) -> Arc<DataBaseRef> {
        std::env::set_var("ARCHIVE_BLOCK_INFO_CACHE", "0");
        if by_number {
            std::env::set_var("ARCHIVE_BLOCK_INDEX", "1");
        }
        let db = Arc::new(DataBaseRef::open(dir, 64, false, false));
        std::env::remove_var("ARCHIVE_BLOCK_INDEX");
        std::env::remove_var("ARCHIVE_BLOCK_INFO_CACHE");
        db
    }

    fn temp_dir(name: &str, v3: bool) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "leafage-archive-block-index-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if v3 {
            std::fs::write(dir.join(udt::SCHEMA_FILE), "3").unwrap();
        }
        dir
    }

    /// Opening a database with CF 2 moves its canonical blocks to CF 7, in
    /// several overlapping files, and drops CF 2. Blocks are then written to
    /// and found in CF 7 alone, with or without the variable, and rewind
    /// truncates it.
    #[test]
    fn moves_block_infos_at_open() {
        let _g = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("move", true);
        {
            let db = open(&dir, false);
            assert!(db.block_index.is_none());
            commit(&db, 1..=60, false);
            commit(&db, [20, 45], true);
            assert_reads(&db, 1..=60);
        }
        assert!(!columns(&dir).contains(&StorageTypeColumn::BlockNumToBlockInfo));

        let db = open(&dir, true);
        let cols = columns(&dir);
        assert!(cols.contains(&StorageTypeColumn::BlockNumToBlockInfo));
        assert!(!cols.contains(&StorageTypeColumn::BlockHashToBlockInfo));
        assert!(!dir.join(".block_index_tmp").exists());
        assert!(dir.join(FILE).exists());
        assert_eq!(records(&db), (1..=60).collect::<Vec<_>>());
        assert_reads(&db, 1..=60);
        assert_eq!(db.read_block_info(hash(20, true)).unwrap(), None);
        assert_eq!(db.read_block_info(hash(61, false)).unwrap(), None);
        drop(db);

        let db = open(&dir, false);
        assert_eq!(db.read_latest_block_num().unwrap(), Some(60));
        // New blocks, merged into the sorted entries along the way.
        for number in 61..=75 {
            commit(&db, [number], false);
        }
        assert!(
            db.block_index
                .as_ref()
                .unwrap()
                .recent
                .read()
                .unwrap()
                .new
                .len()
                < MERGE_AT
        );
        assert_reads(&db, 1..=75);
        let all: Vec<u64> = db
            .block_info_iter()
            .map(|info| info.unwrap().header.number)
            .collect();
        assert_eq!(all, (1..=75).collect::<Vec<_>>());
        drop(db);

        udt::check_rewind_target(&dir, 50).unwrap();
        assert!(udt::check_rewind_target(&dir, 76).is_err());
        let db = open(&dir, false);
        db.rewind_to(&info(50, false), false).unwrap();
        assert_eq!(records(&db), (1..=50).collect::<Vec<_>>());
        drop(db);
        let db = open(&dir, false);
        assert_reads(&db, 1..=50);
        assert_eq!(db.read_block_info(hash(55, false)).unwrap(), None);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A CF 7 left by an interrupted move is dropped, and the move starts
    /// over when asked for.
    #[test]
    fn interrupted_move_starts_over() {
        let _g = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("interrupted", false);
        {
            let db = open(&dir, false);
            commit(&db, 1..=10, false);
        }
        {
            let names: Vec<&str> = ARCHIVE_COLUMNS.iter().map(|c| c.to_str()).collect();
            let mut raw = DB::open_cf(&Options::default(), &dir, names).unwrap();
            raw.create_cf("7", &Options::default()).unwrap();
            let cf = raw.cf_handle("7").unwrap();
            raw.put_cf(cf, key(99), b"partial").unwrap();
        }
        let db = open(&dir, false);
        assert!(db.block_index.is_none());
        assert_eq!(columns(&dir), ARCHIVE_COLUMNS.to_vec());
        assert_reads(&db, 1..=10);
        drop(db);

        let db = open(&dir, true);
        assert_eq!(records(&db), (1..=10).collect::<Vec<_>>());
        assert_reads(&db, 1..=10);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A canonical block without its CF 2 record fails the move and keeps
    /// CF 2.
    #[test]
    fn incomplete_copy_keeps_block_hash_cf() {
        let _g = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("incomplete", false);
        {
            let db = open(&dir, false);
            commit(&db, 1..=10, false);
            let cf = db.db.cf_handle("2").unwrap();
            db.db.delete_cf(cf, hash(4, false)).unwrap();
        }
        std::env::set_var("ARCHIVE_BLOCK_INDEX", "1");
        let opened = DataBaseRef::open_inner(&dir, 64, false, false, false, false);
        std::env::remove_var("ARCHIVE_BLOCK_INDEX");
        let err = opened.unwrap_err().to_string();
        assert!(err.contains("copied 9 of 10"), "{err}");
        assert!(columns(&dir).contains(&StorageTypeColumn::BlockHashToBlockInfo));
        let db = open(&dir, false);
        assert!(db.block_index.is_none());
        assert_reads(&db, (1..=10).filter(|&n| n != 4));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A new database with the variable has no CF 2.
    #[test]
    fn new_database_has_no_block_hash_cf() {
        let _g = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("new", false);
        let db = open(&dir, true);
        assert!(!columns(&dir).contains(&StorageTypeColumn::BlockHashToBlockInfo));
        commit(&db, 1..=5, false);
        assert_reads(&db, 1..=5);
        drop(db);

        let db = open(&dir, false);
        commit(&db, 6..=8, false);
        assert_eq!(records(&db), (1..=8).collect::<Vec<_>>());
        assert_reads(&db, 1..=8);
        let mut batch = db.prepare_write_batch().unwrap();
        assert!(db
            .write_block_info(&mut batch, info(u64::from(u32::MAX) + 1, false))
            .is_err());
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An index of `blocks` merged into the shards, with `k` bucket bits.
    fn index_of(k: u32, blocks: &[(H256, u32)]) -> BlockIndex {
        let empty = (0..SHARDS).map(|_| Shard::from_sorted(k, &[])).collect();
        let index = BlockIndex::new(empty, None);
        index.note(blocks).unwrap();
        index.merge(false);
        index
    }

    #[test]
    fn candidates_check_the_hash_prefix() {
        let a = H256::repeat_byte(0xaa);
        let mut b = a;
        b.0[31] = 0; // same prefix
        let mut c = a;
        c.0[2] = 0; // same shard and bucket (byte 1), another fingerprint
        let d = H256::repeat_byte(0xcc);
        let index = index_of(8, &[(a, 7), (b, 9), (c, 5), (d, 3)]);
        assert_eq!(index.candidates(&a), vec![7, 9]);
        assert_eq!(index.candidates(&c), vec![5]);
        assert_eq!(index.candidates(&d), vec![3]);
        assert!(index.candidates(&H256::repeat_byte(0x01)).is_empty());
        assert_eq!(index.len(), 4);
        // Noted, then merged.
        index.note(&[(d, 12)]).unwrap();
        assert_eq!(index.candidates(&d), vec![3, 12]);
        index.merge(false);
        assert_eq!(index.candidates(&d), vec![3, 12]);
        assert_eq!(*index.last.lock().unwrap(), Some((12, d)));
    }

    /// With the production floor of bucket bits.
    #[test]
    fn candidates_with_16_bucket_bits() {
        let blocks: Vec<(H256, u32)> = (1..=2000).map(|n| (hash(n, false), n as u32)).collect();
        let index = index_of(16, &blocks);
        assert_eq!(index.k(), 16);
        for &(hash, number) in &blocks {
            assert_eq!(index.candidates(&hash), vec![u64::from(number)]);
        }
        let mut other = hash(7, false);
        other.0[3] ^= 1; // same bucket, another fingerprint
        assert!(index.candidates(&other).is_empty());
    }

    #[test]
    fn merge_keeps_entries_sorted_and_unique() {
        let shard = Shard::from_sorted(2, &[(0x002, 4), (0x101, 9), (0x101, 10), (0x3ff, 1)]);
        assert_eq!(shard.offsets, vec![0, 1, 3, 3, 4]);
        let merged = shard.merged(&[(0x001, 7), (0x101, 9), (0x2aa, 2), (0x3ff, 0)]);
        assert_eq!(
            merged.entries().collect::<Vec<_>>(),
            vec![
                (0x001, 7),
                (0x002, 4),
                (0x101, 9),
                (0x101, 10),
                (0x2aa, 2),
                (0x3ff, 0),
                (0x3ff, 1)
            ]
        );
        assert_eq!(merged.offsets, vec![0, 2, 4, 5, 7]);
    }

    #[test]
    fn bucket_bits_keep_about_8_blocks_per_bucket() {
        assert_eq!(bucket_bits(0), 0);
        assert_eq!(bucket_bits(2048), 0);
        assert_eq!(bucket_bits(4096), 1);
        assert_eq!(bucket_bits(131_404_228), 16);
        assert_eq!(bucket_bits(520_866_220), 18);
        assert_eq!(bucket_bits(u64::from(u32::MAX) + 1), 21);
    }

    /// The file round-trips, and a damaged one fails to load.
    #[test]
    fn file_round_trip() {
        let dir = temp_dir("file", false);
        let blocks: Vec<(H256, u32)> = (1..=40).map(|n| (hash(n, false), n as u32)).collect();
        let index = index_of(1, &blocks);
        let file = dir.join(FILE);
        index.save_to(&file).unwrap();
        let (k, shards, last) = load(&file).unwrap().unwrap();
        assert_eq!((k, last), (1, (40, hash(40, false))));
        let loaded = BlockIndex::new(shards, Some(last));
        for &(hash, number) in &blocks {
            assert_eq!(loaded.candidates(&hash), index.candidates(&hash));
            assert!(loaded.contains(&hash, u64::from(number)));
        }

        let mut bytes = std::fs::read(&file).unwrap();
        bytes[100] ^= 1;
        std::fs::write(&file, &bytes).unwrap();
        assert!(load(&file).err().unwrap().to_string().contains("checksum"));
        std::fs::write(&file, &bytes[..bytes.len() - 1]).unwrap();
        assert!(load(&file).is_err());
        std::fs::remove_file(&file).unwrap();
        assert!(load(&file).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An open loads the saved index and indexes the blocks after it; a
    /// damaged file is rebuilt.
    #[test]
    fn open_loads_the_saved_index() {
        let _g = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("load", true);
        {
            let db = open(&dir, true);
            // Saved at 4 and 8, in the background.
            for number in 1..=10 {
                commit(&db, [number], false);
            }
        }
        let (_, _, last) = load(&dir.join(FILE)).unwrap().unwrap();
        assert_eq!(last, (8, hash(8, false)));
        {
            let db = open(&dir, false);
            let index = db.block_index.as_ref().unwrap();
            assert_eq!(*index.last.lock().unwrap(), Some((8, hash(8, false))));
            assert_eq!(index.recent.read().unwrap().new.len(), 2);
            assert_reads(&db, 1..=10);
        }

        let file = dir.join(FILE);
        let mut bytes = std::fs::read(&file).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 1;
        std::fs::write(&file, &bytes).unwrap();
        {
            let db = open(&dir, false);
            let index = db.block_index.as_ref().unwrap();
            assert_eq!(*index.last.lock().unwrap(), Some((10, hash(10, false))));
            assert_reads(&db, 1..=10);
        }
        let (_, _, last) = load(&file).unwrap().unwrap();
        assert_eq!(last, (10, hash(10, false)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Blocks rewritten below the saved index's last one, or another block
    /// at it (after a head-only rewind, CF 3 keeps the higher blocks), drop
    /// the file, so the next open rebuilds and finds them.
    #[test]
    fn rewritten_blocks_drop_the_saved_index() {
        let _g = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("rewrite", true);
        {
            let db = open(&dir, true);
            for number in 1..=8 {
                commit(&db, [number], false);
            }
        }
        assert_eq!(load(&dir.join(FILE)).unwrap().unwrap().2 .0, 8);
        {
            let db = open(&dir, false);
            // Block 5 of another chain, as after a head-only rewind to 4.
            let mut batch = db.prepare_write_batch().unwrap();
            let info = info(5, true);
            db.write_block_hash(&mut batch, 5, info.header.hash)
                .unwrap();
            db.write_latest_block_hash(&mut batch, info.header.hash)
                .unwrap();
            db.write_block_info(&mut batch, info).unwrap();
            db.commit(batch).unwrap();
            assert!(!dir.join(FILE).exists());
        }
        let db = open(&dir, false);
        assert_eq!(
            db.read_block_info(hash(5, true)).unwrap(),
            Some(info(5, true))
        );
        assert_reads(&db, (1..=8).filter(|&n| n != 5));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The head committed again unchanged, as the state tree does on its
    /// first commit after an open, keeps the saved index; another block at
    /// the head's number drops it.
    #[test]
    fn recommitted_head_keeps_the_saved_index() {
        let _g = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("recommit", true);
        {
            let db = open(&dir, true);
            for number in 1..=8 {
                commit(&db, [number], false);
            }
        }
        let file = dir.join(FILE);
        assert_eq!(load(&file).unwrap().unwrap().2 .0, 8);
        {
            let db = open(&dir, false);
            commit(&db, [8, 9], false);
            assert!(file.exists());
            assert_reads(&db, 1..=9);
        }
        {
            // Loaded from the file, with block 9 from CF 3.
            let db = open(&dir, false);
            assert!(file.exists());
            assert_reads(&db, 1..=9);
            rewrite(&db, 8).unwrap();
            assert!(!file.exists());
        }
        let db = open(&dir, false);
        assert_eq!(
            db.read_block_info(hash(8, true)).unwrap(),
            Some(info(8, true))
        );
        assert_reads(&db, (1..=9).filter(|&n| n != 8));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Commit block `number` of another chain, as after a head-only rewind
    /// below it.
    fn rewrite(db: &Arc<DataBaseRef>, number: u64) -> Result<(), Error> {
        let mut batch = db.prepare_write_batch().unwrap();
        let info = info(number, true);
        db.write_block_hash(&mut batch, number, info.header.hash)
            .unwrap();
        db.write_latest_block_hash(&mut batch, info.header.hash)
            .unwrap();
        db.write_block_info(&mut batch, info).unwrap();
        db.commit(batch)
    }

    /// A file that fails to go keeps the rewrite pending: the next commit
    /// below it drops the file again.
    #[test]
    fn failed_drop_is_retried() {
        let _g = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("retry", true);
        let db = open(&dir, true);
        for number in 1..=8 {
            commit(&db, [number], false);
        }
        db.block_index.as_ref().unwrap().wait_saved();
        let file = dir.join(FILE);
        let saved = std::fs::read(&file).unwrap();
        // A directory in its place fails the removal.
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(rewrite(&db, 5).is_err());
        std::fs::remove_dir(&file).unwrap();
        std::fs::write(&file, saved).unwrap();
        rewrite(&db, 5).unwrap();
        assert!(!file.exists());
        drop(db);
        let db = open(&dir, false);
        assert_eq!(
            db.read_block_info(hash(5, true)).unwrap(),
            Some(info(5, true))
        );
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A move that failed leaves a file that the archive, back on CF 2, does
    /// not keep up to date; the next move rebuilds the index instead.
    #[test]
    fn move_rebuilds_the_index() {
        let _g = super::super::ARCHIVE_DB_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("remove", true);
        {
            let db = open(&dir, false);
            commit(&db, 1..=10, false);
            let cf = db.db.cf_handle("2").unwrap();
            db.db.delete_cf(cf, hash(4, false)).unwrap();
        }
        std::env::set_var("ARCHIVE_BLOCK_INDEX", "1");
        let opened = DataBaseRef::open_inner(&dir, 64, false, false, false, false);
        std::env::remove_var("ARCHIVE_BLOCK_INDEX");
        assert!(opened.is_err());
        assert!(dir.join(FILE).exists());
        {
            let db = open(&dir, false);
            assert!(db.block_index.is_none());
            rewrite(&db, 5).unwrap();
            commit(&db, [4], false);
        }
        let db = open(&dir, true);
        assert_eq!(
            db.read_block_info(hash(5, true)).unwrap(),
            Some(info(5, true))
        );
        assert_reads(&db, (1..=10).filter(|&n| n != 5));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
