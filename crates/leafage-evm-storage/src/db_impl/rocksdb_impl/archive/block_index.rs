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
//! The index is built at open from `BlockNumToBlockHash`: 8 bytes per block
//! (~1GB for bsc). Block numbers must fit in a `u32`.

use super::*;
use crate::db_impl::rocksdb_impl::sst::{ingest_external_file_options, sst_writer_options};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Instant;

/// The move sorts CF `2` records by number in chunks of this size.
const CHUNK_BYTES: usize = if cfg!(test) { 4096 } else { 1 << 30 };

/// Entries of new blocks are merged into the sorted ones in batches of this
/// size: a merge copies the whole index.
const MERGE_AT: usize = if cfg!(test) { 4 } else { 1 << 20 };

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

#[inline]
fn prefix(hash: &H256) -> u64 {
    u64::from(u32::from_be_bytes(hash[..4].try_into().unwrap()))
}

/// An index entry: the hash's first 4 bytes above the number.
#[inline]
pub(super) fn entry(hash: &H256, number: u64) -> Result<u64, Error> {
    let number = u32::try_from(number).map_err(|_| {
        Error::UnSupported(format!("block {number} is past the archive block index"))
    })?;
    Ok(prefix(hash) << 32 | u64::from(number))
}

/// Merge two sorted entry lists, dropping duplicates.
fn merge(sorted: &[u64], recent: &BTreeSet<u64>) -> Vec<u64> {
    let mut merged = Vec::with_capacity(sorted.len() + recent.len());
    let mut recent = recent.iter().copied().peekable();
    for &e in sorted {
        while let Some(r) = recent.next_if(|&r| r <= e) {
            if r < e {
                merged.push(r);
            }
        }
        merged.push(e);
    }
    merged.extend(recent);
    merged
}

pub(super) struct BlockIndex {
    /// Sorted entries: the canonical blocks at open, then merged with
    /// `recent` as it grows.
    sorted: RwLock<Arc<Vec<u64>>>,
    /// Entries of the blocks committed since the last merge.
    recent: RwLock<BTreeSet<u64>>,
}

impl Debug for BlockIndex {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        // One lock at a time: `note` takes `recent`, then `sorted`.
        let sorted = self.sorted.read().unwrap().len();
        let recent = self.recent.read().unwrap().len();
        f.debug_struct("BlockIndex")
            .field("sorted", &sorted)
            .field("recent", &recent)
            .finish()
    }
}

impl BlockIndex {
    fn from_sorted(sorted: Vec<u64>) -> Self {
        Self {
            sorted: RwLock::new(Arc::new(sorted)),
            recent: RwLock::new(BTreeSet::new()),
        }
    }

    /// Index every block of `BlockNumToBlockHash`.
    pub(super) fn build(db: &DB) -> Result<Self, Error> {
        let start = Instant::now();
        let cf = db
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .unwrap();
        let blocks = match db.iterator_cf(cf, IteratorMode::End).next() {
            Some(item) => block_number(&item?.0) + 1,
            None => 0,
        };
        let mut entries = Vec::with_capacity(blocks as usize);
        for item in db.iterator_cf_opt(cf, scan_options(), IteratorMode::Start) {
            let (number, hash) = item?;
            entries.push(entry(&H256::from_slice(&hash), block_number(&number))?);
        }
        entries.sort_unstable();
        info!(target: "rocksdb", blocks = entries.len(), secs = start.elapsed().as_secs(),
            "archive block index built");
        Ok(Self::from_sorted(entries))
    }

    /// The numbers that may hold the block `hash`.
    pub(super) fn candidates(&self, hash: &H256) -> Vec<u64> {
        let lo = prefix(hash) << 32;
        let hi = lo | u64::from(u32::MAX);
        let number = |e: u64| e & u64::from(u32::MAX);
        let mut numbers: Vec<u64> = self
            .recent
            .read()
            .unwrap()
            .range(lo..=hi)
            .map(|&e| number(e))
            .collect();
        let sorted = self.sorted.read().unwrap().clone();
        let start = sorted.partition_point(|&e| e < lo);
        for &e in sorted[start..].iter().take_while(|&&e| e <= hi) {
            if !numbers.contains(&number(e)) {
                numbers.push(number(e));
            }
        }
        numbers
    }

    /// Index a block about to be committed. The record carries its hash, so
    /// an entry ahead of its write only misses.
    pub(super) fn note(&self, e: u64) {
        let mut recent = self.recent.write().unwrap();
        recent.insert(e);
        if recent.len() >= MERGE_AT {
            let sorted = self.sorted.read().unwrap().clone();
            let merged = merge(&sorted, &recent);
            *self.sorted.write().unwrap() = Arc::new(merged);
            recent.clear();
        }
    }
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
    let canonical = index.sorted.read().unwrap().clone();
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
        if !entry(&hash, number).is_ok_and(|e| canonical.binary_search(&e).is_ok()) {
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
    if records != canonical.len() {
        return Err(Error::UnSupported(format!(
            "moving block infos: copied {records} of {} canonical blocks",
            canonical.len()
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

    #[test]
    fn candidates_check_the_hash_prefix() {
        let a = H256::repeat_byte(0xaa);
        let mut b = a;
        b.0[31] = 0; // same 4-byte prefix
        let c = H256::repeat_byte(0xcc);
        let mut sorted: Vec<u64> = [(a, 7), (b, 9), (c, 3)]
            .iter()
            .map(|(h, n)| entry(h, *n).unwrap())
            .collect();
        sorted.sort_unstable();
        let index = BlockIndex::from_sorted(sorted);
        assert_eq!(index.candidates(&a), vec![7, 9]);
        assert_eq!(index.candidates(&c), vec![3]);
        assert!(index.candidates(&H256::repeat_byte(0x01)).is_empty());
        index.note(entry(&c, 4).unwrap());
        assert_eq!(index.candidates(&c), vec![4, 3]);
    }

    #[test]
    fn merge_keeps_entries_sorted_and_unique() {
        let recent: BTreeSet<u64> = [1, 4, 9, 12].into_iter().collect();
        assert_eq!(merge(&[2, 4, 10], &recent), vec![1, 2, 4, 9, 10, 12]);
        assert_eq!(merge(&[], &recent), vec![1, 4, 9, 12]);
        assert_eq!(merge(&[3], &BTreeSet::new()), vec![3]);
    }
}
