//! The layered store of archive schema v4: a memtable and immutable delta
//! segments over immutable base files, with the background work that moves
//! versions down (`docs/ArchiveSchemaV4.md`, "Store").
//!
//! Readers work on a [`View`], an immutable snapshot of the layers swapped
//! atomically; files stay open as long as a view holds them, so removing a
//! merged file never disturbs a reader. Every change of the file set is
//! recorded in `MANIFEST` (written aside, synced, renamed) before the view
//! shows it. Files not in the manifest are leftovers of an interrupted
//! write and are removed at open.

use super::super::udt::hex;
use super::codec::{self, key_len, Look, Next, Val};
use super::corrupt;
use super::file::{
    self, bloom_path, remove_page_file, sync_dir, write_file_synced, Bloom, Cursor, Dict,
    MergeIter, PageFile, PageWriter, Rec, SeqReader,
};
use super::mem::Mem;
use super::side::{
    self, BlockRec, Blocks, BlocksAppend, BlocksMeta, Codes, CodesAppend, CodesMeta,
};
use crate::db_impl::error::Error;
use crate::metrics::ARCHIVE_V4_METRICS as M;
use arc_swap::ArcSwap;
use quick_cache::sync::{Cache as QuickCache, GuardResult};
use quick_cache::Weighter;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

const MANIFEST: &str = "MANIFEST";
const LOCK: &str = "LOCK";
const MANIFEST_MAGIC: &str = "leafage-archive-v4 1";
/// Dictionary id of "no dictionary".
const NO_DICT: u32 = 0;
/// zstd level of delta segments: they are rewritten into the base soon.
const SEGMENT_LEVEL: i32 = 3;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

/// Tuning of a [`Store`].
#[derive(Clone, Debug)]
pub(crate) struct Options {
    /// Memory for the page index, the filters and the page cache together.
    pub(crate) budget_bytes: u64,
    /// The memtable becomes a segment at this size...
    pub(crate) flush_bytes: u64,
    /// ...or once it spans this many blocks.
    pub(crate) flush_blocks: u64,
    /// Segments of one level merged into one of the next.
    pub(crate) tiers: usize,
    /// A base file is closed at the first key boundary past this size.
    pub(crate) base_file_bytes: u64,
    /// zstd level of base files.
    pub(crate) base_level: i32,
    /// Device bytes per second segment merges and base rewrites may read
    /// and write, together; 0: no cap.
    pub(crate) rewrite_bytes_per_sec: f64,
    /// A base rewrite starts when the delta exceeds this share of the base...
    pub(crate) delta_ratio: f64,
    /// ...and this size...
    pub(crate) delta_min_bytes: u64,
    /// ...or when the segment filters take this much memory.
    pub(crate) delta_filter_bytes: u64,
    /// Memory for decoded chunks of block infos.
    pub(crate) block_cache_bytes: u64,
    /// How often the background looks whether a base rewrite is due.
    pub(crate) rewrite_check: Duration,
    /// Run flushes, merges and base rewrites on background threads. Without
    /// it a flush runs inside the write that fills the memtable, and the
    /// rest only when called.
    pub(crate) background: bool,
}

impl Options {
    /// A serving node: `budget_bytes` of memory, tuned by `ARCHIVE_V4_*`.
    pub(crate) fn serving(budget_bytes: u64) -> Self {
        Self {
            budget_bytes,
            flush_bytes: env_u64("ARCHIVE_V4_FLUSH_MB", 512) << 20,
            flush_blocks: env_u64("ARCHIVE_V4_FLUSH_BLOCKS", 4800).max(1),
            tiers: env_u64("ARCHIVE_V4_TIERS", 8).max(2) as usize,
            base_file_bytes: (env_f64("ARCHIVE_V4_BASE_FILE_MB", 2048.0) * (1 << 20) as f64) as u64,
            base_level: env_u64("ARCHIVE_V4_BASE_ZSTD_LEVEL", 3) as i32,
            rewrite_bytes_per_sec: env_f64("ARCHIVE_V4_REWRITE_MBPS", 32.0) * 1e6,
            delta_ratio: env_f64("ARCHIVE_V4_DELTA_PERCENT", 4.0) / 100.0,
            delta_min_bytes: env_u64("ARCHIVE_V4_DELTA_MIN_MB", 4096) << 20,
            delta_filter_bytes: env_u64("ARCHIVE_V4_DELTA_FILTER_MB", 1024) << 20,
            block_cache_bytes: env_u64("ARCHIVE_V4_BLOCK_CACHE_MB", 64) << 20,
            rewrite_check: Duration::from_secs_f64(
                env_f64("ARCHIVE_V4_REWRITE_CHECK_SECS", 30.0).max(0.01),
            ),
            background: true,
        }
    }

    /// Bulk load: nobody reads, so the base is rewritten unthrottled and
    /// only when the delta has grown to half of it.
    pub(crate) fn bulk_load(budget_bytes: u64) -> Self {
        Self {
            rewrite_bytes_per_sec: 0.0,
            delta_ratio: 0.5,
            delta_filter_bytes: u64::MAX,
            ..Self::serving(budget_bytes)
        }
    }

    /// Offline tools: no background work.
    pub(crate) fn maintenance(budget_bytes: u64) -> Self {
        Self {
            rewrite_bytes_per_sec: 0.0,
            background: false,
            ..Self::serving(budget_bytes)
        }
    }
}

fn kind_tag(kind: usize) -> char {
    if kind == 1 {
        's'
    } else {
        'a'
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok())
        .collect()
}

/// A base file: every version of its keys up to `hi`.
struct BaseFile {
    file: Arc<PageFile>,
    /// File name without extension, relative to the store directory.
    name: String,
    hi: u64,
    records: u64,
    /// First key of the file, zero-padded. A base file covers the keys from
    /// this one up to the next file's.
    first_key: [u8; 64],
}

/// A delta segment: every version of the blocks `lo..=hi`.
struct Seg {
    file: Arc<PageFile>,
    name: String,
    lo: u64,
    hi: u64,
    level: u32,
    records: u64,
    distinct: u64,
    bloom: Bloom,
}

/// An immutable snapshot of the store's layers.
#[derive(Clone)]
struct View {
    active: Arc<Mem>,
    /// The memtable being written to a segment.
    frozen: Option<Arc<Mem>>,
    /// Per kind (0 accounts, 1 storage), oldest first.
    segs: [Vec<Arc<Seg>>; 2],
    /// Per kind, in key order.
    base: [Vec<Arc<BaseFile>>; 2],
    /// The valid part of the block and code files.
    blocks: BlocksMeta,
    codes: CodesMeta,
}

impl View {
    fn base_index(&self, kind: usize, key: &[u8]) -> Option<usize> {
        let k = key.len();
        self.base[kind]
            .partition_point(|b| b.first_key[..k] <= *key)
            .checked_sub(1)
    }

    /// The last block held by the files, if they hold any.
    fn durable(&self) -> Option<u64> {
        self.segs
            .iter()
            .flatten()
            .map(|s| s.hi)
            .chain(self.base.iter().flatten().map(|b| b.hi))
            .chain(self.blocks.last())
            .max()
    }

    /// Segments of `kind` not yet in every base file: the delta.
    fn delta(&self, kind: usize) -> impl Iterator<Item = &Arc<Seg>> {
        let min_hi = self.base[kind].iter().map(|b| b.hi).min();
        self.segs[kind]
            .iter()
            .filter(move |s| min_hi.is_none_or(|m| s.hi > m))
    }
}

#[derive(Clone, Copy)]
struct PageWeighter;

impl Weighter<u64, Arc<[u8]>> for PageWeighter {
    /// What the page takes in memory: the allocator rounds a size up to
    /// one of four classes per power of two (jemalloc), and the cache and
    /// the `Arc` add their own.
    fn weight(&self, _: &u64, v: &Arc<[u8]>) -> u64 {
        let len = (v.len() as u64 + 16).max(64);
        let step = (len - 1).next_power_of_two() / 8;
        len.next_multiple_of(step) + 96
    }
}

type PageCache = QuickCache<u64, Arc<[u8]>, PageWeighter>;

/// State behind the publication lock.
struct State {
    next_fid: u32,
    next_dict: u32,
    dicts: HashMap<u32, Arc<Dict>>,
    /// Dictionary of new segments, per kind.
    segdict: [u32; 2],
    /// Per kind, the block a base rewrite pass under way folds the delta
    /// up to (0: no pass). It is in the manifest, so that a pass cut short
    /// goes on with the files it had not reached.
    pass: [u64; 2],
    /// The block a rewind under way goes back to. It is in the manifest
    /// while files are trimmed one by one: between two of them the store
    /// holds some keys up to that block and others beyond it.
    rewinding: Option<u64>,
}

/// A result of [`Store::get`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Read {
    /// The greatest version at or below the read height: block and value.
    pub(crate) found: Option<(u64, Val)>,
    /// The answer holds for every height below this block: the key has no
    /// version in between. `u64::MAX`: none up to the store's head. It may
    /// be lower than the key's real next version, never higher.
    pub(crate) next: u64,
}

struct Inner {
    dir: PathBuf,
    opts: Options,
    cache: PageCache,
    view: ArcSwap<View>,
    /// One writer at a time.
    write: Mutex<()>,
    /// Publication of views and the manifest.
    state: Mutex<State>,
    /// Next block expected by `apply`: one above the last applied.
    next_block: AtomicU64,
    /// Wakes the flusher (a memtable was frozen) and writers waiting on it.
    flush_lock: Mutex<()>,
    flush_cv: Condvar,
    /// One base rewrite pass, and one segment merge, at a time.
    rewriting: Mutex<()>,
    merging: Mutex<()>,
    /// Wakes the rewriter early.
    rewrite_lock: Mutex<bool>,
    rewrite_cv: Condvar,
    /// The merger sleeps here until a flush adds a segment.
    merge_lock: Mutex<bool>,
    merge_cv: Condvar,
    /// When the background IO accounted so far is paid for: see
    /// [`Throttle`].
    paced: Mutex<Instant>,
    stop: AtomicBool,
    /// A background flush or merge failed; writes report it.
    failed: Mutex<Option<String>>,
    /// Block infos and code: see [`side`].
    blocks: Blocks,
    codes: Codes,
    /// Held while the store is open: one process at a time.
    _lock: std::fs::File,
}

/// The versions of one block: its number, accounts and slots.
pub(crate) type BlockVersions<'a> = (u64, &'a [([u8; 32], Val)], &'a [([u8; 64], [u8; 32])]);

/// See the module documentation.
pub(crate) struct Store {
    inner: Arc<Inner>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("v4::Store")
            .field("dir", &self.inner.dir)
            .field("head", &self.head())
            .finish()
    }
}

/// A base file listed for a new store by [`Store::create_with_base`].
pub(crate) struct NewBase {
    /// File id, unique among the files listed.
    pub(crate) fid: u32,
    pub(crate) storage: bool,
    /// File name without extension, in the store directory.
    pub(crate) name: String,
    /// Dictionary file name in the store directory; `None`: no dictionary.
    pub(crate) dict: Option<String>,
    pub(crate) hi: u64,
    pub(crate) records: u64,
    pub(crate) first_key: [u8; 64],
}

struct ManifestData {
    next_fid: u32,
    next_dict: u32,
    /// id -> file name.
    dicts: Vec<(u32, String)>,
    segdict: [u32; 2],
    /// The target of a base rewrite pass under way, per kind (0: none).
    pass: [u64; 2],
    /// The block a rewind under way goes back to.
    rewinding: Option<u64>,
    /// kind, fid, dict, hi, records, name, first key.
    base: Vec<(usize, u32, u32, u64, u64, String, [u8; 64])>,
    /// kind, fid, dict, lo, hi, level, records, distinct, name.
    segs: Vec<(usize, u32, u32, u64, u64, u32, u64, u64, String)>,
    blocks: BlocksMeta,
    codes: CodesMeta,
}

impl ManifestData {
    fn empty() -> Self {
        Self {
            next_fid: 1,
            next_dict: 1,
            dicts: Vec::new(),
            segdict: [NO_DICT; 2],
            pass: [0; 2],
            rewinding: None,
            base: Vec::new(),
            segs: Vec::new(),
            blocks: BlocksMeta::default(),
            codes: CodesMeta::default(),
        }
    }

    fn parse(text: &str) -> Result<Self, Error> {
        let mut lines = text.lines();
        if lines.next() != Some(MANIFEST_MAGIC) {
            return Err(corrupt("unknown manifest version"));
        }
        let mut m = Self::empty();
        for line in lines {
            let bad = || corrupt(format!("manifest line {line:?}"));
            let f: Vec<&str> = line.split_whitespace().collect();
            let num = |i: usize| -> Result<u64, Error> {
                f.get(i).and_then(|s| s.parse().ok()).ok_or_else(bad)
            };
            let text = |i: usize| -> Result<String, Error> {
                f.get(i).map(|s| s.to_string()).ok_or_else(bad)
            };
            match f.first().copied() {
                None => {}
                Some("next_fid") => m.next_fid = num(1)? as u32,
                Some("next_dict") => m.next_dict = num(1)? as u32,
                Some("dict") => m.dicts.push((num(1)? as u32, text(2)?)),
                Some("segdict") => {
                    let kind = num(1)? as usize;
                    *m.segdict.get_mut(kind).ok_or_else(bad)? = num(2)? as u32;
                }
                Some("pass") => {
                    let kind = num(1)? as usize;
                    *m.pass.get_mut(kind).ok_or_else(bad)? = num(2)?;
                }
                Some("rewinding") => m.rewinding = Some(num(1)?),
                Some("base") => {
                    let key = f.get(7).and_then(|s| unhex(s)).ok_or_else(bad)?;
                    let kind = num(1)? as usize;
                    if kind > 1 || key.len() != key_len(kind == 1) {
                        return Err(bad());
                    }
                    let mut first_key = [0u8; 64];
                    first_key[..key.len()].copy_from_slice(&key);
                    m.base.push((
                        kind,
                        num(2)? as u32,
                        num(3)? as u32,
                        num(4)?,
                        num(5)?,
                        text(6)?,
                        first_key,
                    ));
                }
                Some("seg") => {
                    let kind = num(1)? as usize;
                    if kind > 1 {
                        return Err(bad());
                    }
                    m.segs.push((
                        kind,
                        num(2)? as u32,
                        num(3)? as u32,
                        num(4)?,
                        num(5)?,
                        num(6)? as u32,
                        num(7)?,
                        num(8)?,
                        text(9)?,
                    ));
                }
                // Address filters, which earlier builds kept: their files
                // go with the other leftovers at this open.
                Some("filter") => {}
                Some("blocks") => {
                    m.blocks = BlocksMeta {
                        first: num(1)?,
                        count: num(2)?,
                        dat_len: num(3)?,
                        tail: num(4)?,
                        dict: num(5)? == 1,
                    }
                }
                Some("codes") => {
                    m.codes = CodesMeta {
                        count: num(1)?,
                        dat_len: num(2)?,
                        dict: num(3)? == 1,
                    }
                }
                Some(_) => return Err(bad()),
            }
        }
        Ok(m)
    }

    fn render(&self) -> String {
        let mut s = format!(
            "{MANIFEST_MAGIC}\nnext_fid {}\nnext_dict {}\n",
            self.next_fid, self.next_dict
        );
        for (id, name) in &self.dicts {
            s += &format!("dict {id} {name}\n");
        }
        for (kind, id) in self.segdict.iter().enumerate() {
            s += &format!("segdict {kind} {id}\n");
        }
        for (kind, target) in self.pass.iter().enumerate() {
            if *target != 0 {
                s += &format!("pass {kind} {target}\n");
            }
        }
        if let Some(block) = self.rewinding {
            s += &format!("rewinding {block}\n");
        }
        for (kind, fid, dict, hi, records, name, first_key) in &self.base {
            s += &format!(
                "base {kind} {fid} {dict} {hi} {records} {name} {}\n",
                hex(&first_key[..key_len(*kind == 1)])
            );
        }
        for (kind, fid, dict, lo, hi, level, records, distinct, name) in &self.segs {
            s +=
                &format!("seg {kind} {fid} {dict} {lo} {hi} {level} {records} {distinct} {name}\n");
        }
        let b = &self.blocks;
        s += &format!(
            "blocks {} {} {} {} {}\n",
            b.first, b.count, b.dat_len, b.tail, b.dict as u8
        );
        s += &format!(
            "codes {} {} {}\n",
            self.codes.count, self.codes.dat_len, self.codes.dict as u8
        );
        s
    }

    fn write(&self, dir: &Path) -> Result<(), Error> {
        let tmp = dir.join(format!("{MANIFEST}.tmp"));
        write_file_synced(&tmp, self.render().as_bytes())?;
        std::fs::rename(&tmp, dir.join(MANIFEST))?;
        sync_dir(dir)
    }
}

fn dict_file(id: u32) -> String {
    format!("dict-{id:06}.dict")
}

/// Paces background IO (segment merges and base rewrites) to a byte rate,
/// one budget for all of it. Time in which less was done than the rate
/// allows (the work was slower, or there was none) gives no credit beyond
/// [`THROTTLE_BURST`]: it is not made up for at the device's speed
/// afterwards.
struct Throttle<'a> {
    inner: &'a Inner,
}

/// The IO a throttle lets through at once, as time at its rate.
const THROTTLE_BURST: Duration = Duration::from_millis(200);

impl<'a> Throttle<'a> {
    fn new(inner: &'a Inner) -> Self {
        Self { inner }
    }

    /// Account `bytes` of device IO, then wait as the pace requires.
    /// Returns an error once the store is closing.
    fn spend(&mut self, bytes: u64) -> Result<(), Error> {
        let rate = self.inner.opts.rewrite_bytes_per_sec;
        let until = (rate > 0.0).then(|| {
            // When the background IO accounted so far is paid for.
            let mut paid = self.inner.paced.lock().unwrap();
            let now = Instant::now();
            let from = (*paid).max(now.checked_sub(THROTTLE_BURST).unwrap_or(now));
            *paid = from + Duration::from_secs_f64(bytes as f64 / rate);
            *paid
        });
        loop {
            if self.inner.stop.load(Ordering::Relaxed) {
                return Err(Error::ArchiveV4("store is closing".into()));
            }
            let wait = until.map_or(Duration::ZERO, |until| {
                until.saturating_duration_since(Instant::now())
            });
            if wait.is_zero() {
                return Ok(());
            }
            std::thread::sleep(wait.min(Duration::from_millis(200)));
        }
    }
}

/// A page of the file being rewritten: as stored, decompressed, and its
/// first record and record count.
#[derive(Default)]
struct Ahead {
    stored: Vec<u8>,
    raw: Vec<u8>,
    key: Key64,
    block: u64,
    records: u64,
}

/// A zero-padded key.
#[derive(Clone, Copy)]
struct Key64([u8; 64]);

impl Default for Key64 {
    fn default() -> Self {
        Self([0; 64])
    }
}

impl std::ops::Deref for Key64 {
    type Target = [u8; 64];
    fn deref(&self) -> &[u8; 64] {
        &self.0
    }
}

impl Ahead {
    fn load(&mut self, src: &mut SeqReader, c: usize, storage: bool) -> Result<(), Error> {
        self.stored.clear();
        self.stored.extend_from_slice(src.stored(c)?);
        self.raw.clear();
        self.raw.extend_from_slice(src.page(c)?);
        let (key, block, records) = codec::first_record(&self.raw, storage)?;
        (self.key, self.block, self.records) = (Key64(key), block, records as u64);
        Ok(())
    }
}

/// Writes the base files of one rewrite, closing a file at the first key
/// boundary past the target size.
struct BaseOut<'a> {
    inner: &'a Inner,
    storage: bool,
    dict: Arc<Dict>,
    hi: u64,
    writer: Option<(u32, String, PageWriter)>,
    done: Vec<Arc<BaseFile>>,
    written: u64,
}

impl<'a> BaseOut<'a> {
    fn new(inner: &'a Inner, storage: bool, dict: Arc<Dict>, hi: u64) -> Self {
        Self {
            inner,
            storage,
            dict,
            hi,
            writer: None,
            done: Vec::new(),
            written: 0,
        }
    }

    fn bytes(&self) -> u64 {
        self.written + self.writer.as_ref().map_or(0, |w| w.2.bytes())
    }

    fn writer(&mut self) -> Result<&mut PageWriter, Error> {
        if self.writer.is_none() {
            let fid = self.inner.new_fid();
            let name = format!("base-{fid:08}-{}", kind_tag(self.storage as usize));
            let w = PageWriter::new(
                &self.inner.dir.join(&name),
                self.storage,
                &self.dict,
                self.inner.opts.base_level,
                None,
            )?;
            self.writer = Some((fid, name, w));
        }
        Ok(&mut self.writer.as_mut().unwrap().2)
    }

    /// Whether the open file is past its target size, so the next page
    /// should be added record by record to find a key boundary.
    fn wants_cut(&self) -> bool {
        self.writer
            .as_ref()
            .is_some_and(|w| w.2.bytes() >= self.inner.opts.base_file_bytes)
    }

    fn open_page_is_short(&self) -> bool {
        self.writer
            .as_ref()
            .is_some_and(|w| w.2.open_page_is_short())
    }

    fn add(&mut self, key: &[u8], block: u64, val: &Val) -> Result<(), Error> {
        if self.wants_cut() && self.writer.as_ref().unwrap().2.can_cut_before(key) {
            self.close()?;
        }
        self.writer()?.add(key, block, val)
    }

    fn copy_page(
        &mut self,
        first_key: &[u8],
        first_block: u64,
        stored: &[u8],
        records: u64,
    ) -> Result<(), Error> {
        self.writer()?
            .copy_page(first_key, first_block, stored, records)
    }

    fn close(&mut self) -> Result<(), Error> {
        let Some((fid, name, w)) = self.writer.take() else {
            return Ok(());
        };
        let path = self.inner.dir.join(&name);
        let opened = w.finish().and_then(|out| match out.first_key {
            Some(first_key) => PageFile::open(&path, fid, self.storage, self.dict.clone())
                .map(|file| Some((out, first_key, file))),
            None => Ok(None),
        });
        let (out, first_key, file) = match opened {
            Ok(Some(opened)) => opened,
            // Nothing in it, or it could not be finished: not a file to keep.
            Ok(None) => {
                remove_page_file(&path);
                return Ok(());
            }
            Err(e) => {
                remove_page_file(&path);
                return Err(e);
            }
        };
        self.written += out.bytes;
        self.done.push(Arc::new(BaseFile {
            file: Arc::new(file),
            name,
            hi: self.hi,
            records: out.records,
            first_key,
        }));
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<Arc<BaseFile>>, Error> {
        if let Err(e) = self.close() {
            self.abort();
            return Err(e);
        }
        Ok(std::mem::take(&mut self.done))
    }

    /// Remove what was written, after a failure.
    fn abort(mut self) {
        if let Some((_, name, w)) = self.writer.take() {
            drop(w);
            remove_page_file(&self.inner.dir.join(name));
        }
        for b in &self.done {
            remove_page_file(&self.inner.dir.join(&b.name));
        }
    }
}

impl Inner {
    fn new_fid(&self) -> u32 {
        let mut st = self.state.lock().unwrap();
        st.next_fid += 1;
        st.next_fid - 1
    }

    fn check_failed(&self) -> Result<(), Error> {
        match &*self.failed.lock().unwrap() {
            Some(e) => Err(Error::ArchiveV4(format!("background work failed: {e}"))),
            None => Ok(()),
        }
    }

    /// Record the view in the manifest, then show it to readers. `side`:
    /// the appends to the block and code files that the view's `blocks` and
    /// `codes` stand for.
    fn publish(
        &self,
        st: &mut State,
        v: View,
        side: Option<(BlocksAppend, CodesAppend)>,
    ) -> Result<(), Error> {
        let mut m = ManifestData::empty();
        m.blocks = v.blocks;
        m.codes = v.codes;
        m.next_fid = st.next_fid;
        m.next_dict = st.next_dict;
        m.segdict = st.segdict;
        m.pass = st.pass;
        m.rewinding = st.rewinding;
        let mut used: HashSet<u32> = st.segdict.iter().copied().collect();
        for (kind, list) in v.base.iter().enumerate() {
            for b in list {
                used.insert(b.file.dict.id);
                m.base.push((
                    kind,
                    b.file.fid,
                    b.file.dict.id,
                    b.hi,
                    b.records,
                    b.name.clone(),
                    b.first_key,
                ));
            }
        }
        for (kind, list) in v.segs.iter().enumerate() {
            for s in list {
                used.insert(s.file.dict.id);
                m.segs.push((
                    kind,
                    s.file.fid,
                    s.file.dict.id,
                    s.lo,
                    s.hi,
                    s.level,
                    s.records,
                    s.distinct,
                    s.name.clone(),
                ));
            }
        }
        used.remove(&NO_DICT);
        let mut dropped = Vec::new();
        st.dicts.retain(|id, _| {
            let keep = used.contains(id) || *id == NO_DICT;
            if !keep {
                dropped.push(*id);
            }
            keep
        });
        let mut ids: Vec<u32> = used.into_iter().collect();
        ids.sort_unstable();
        m.dicts = ids.into_iter().map(|id| (id, dict_file(id))).collect();
        m.write(&self.dir)?;
        // Before the view that no longer has their memtable.
        if let Some((blocks, codes)) = side {
            self.blocks.publish(blocks)?;
            self.codes.publish(codes);
        }
        self.set_gauges(&v);
        self.view.store(Arc::new(v));
        for id in dropped {
            let _ = std::fs::remove_file(self.dir.join(dict_file(id)));
        }
        Ok(())
    }

    fn set_gauges(&self, v: &View) {
        let segs = v.segs.iter().flatten();
        M.segments.set(segs.clone().count() as f64);
        M.delta_bytes.set(
            (0..2)
                .flat_map(|k| v.delta(k))
                .map(|s| s.file.data_len())
                .sum::<u64>() as f64,
        );
        M.base_bytes.set(
            v.base
                .iter()
                .flatten()
                .map(|b| b.file.data_len())
                .sum::<u64>() as f64,
        );
        M.base_files.set(v.base.iter().flatten().count() as f64);
        M.index_bytes.set(index_bytes(v) as f64);
        M.segment_filter_bytes
            .set(segs.map(|s| s.bloom.mem_bytes()).sum::<u64>() as f64);
        if let Some(d) = v.durable() {
            M.durable_block.set(d as f64);
        }
    }

    /// A stored dictionary under a new id.
    fn add_dict(&self, st: &mut State, raw: Vec<u8>) -> Result<Arc<Dict>, Error> {
        if raw.is_empty() {
            return Ok(st.dicts[&NO_DICT].clone());
        }
        let id = st.next_dict;
        st.next_dict += 1;
        write_file_synced(&self.dir.join(dict_file(id)), &raw)?;
        let d = Arc::new(Dict::new(id, raw));
        st.dicts.insert(id, d.clone());
        Ok(d)
    }

    fn page(&self, f: &PageFile, c: usize) -> Result<Arc<[u8]>, Error> {
        let ck = ((f.fid as u64) << 32) | c as u64;
        match self.cache.get_value_or_guard(&ck, None) {
            GuardResult::Value(v) => Ok(v),
            GuardResult::Guard(g) => {
                let page = f.read_page(c)?;
                let _ = g.insert(page.clone());
                Ok(page)
            }
            GuardResult::Timeout => unreachable!("no timeout given"),
        }
    }

    /// The greatest version `<= h` of `key` in one page file, and the block
    /// below which that answer holds as far as this file knows.
    fn file_get(
        &self,
        f: &PageFile,
        key: &[u8],
        h: u64,
    ) -> Result<(Option<(u64, Val)>, u64), Error> {
        // "Unknown": the answer is only known to hold at `h` itself.
        let unknown = h.saturating_add(1);
        let Some(mut c) = f.index.floor(key, h) else {
            // Below the file's first record.
            let next = match f.index.first() {
                Some((first, block)) if first == key => block,
                _ => u64::MAX,
            };
            return Ok((None, next));
        };
        let (mut back, mut fwd) = (false, false);
        loop {
            let page = self.page(f, c)?;
            let (look, next) = codec::lookup(&page, f.storage, key, h)?;
            match look {
                Look::Found(block, val) => {
                    let next = match next {
                        Next::At(b) => b,
                        Next::None => u64::MAX,
                        // The answer is the last record of its page: the
                        // index knows how the next page starts.
                        Next::PageEnd => f.index.continues_at(c, key).unwrap_or(u64::MAX),
                    };
                    return Ok((Some((block, val)), next.max(unknown)));
                }
                Look::Absent => {
                    let next = match next {
                        Next::At(b) => b,
                        Next::None => u64::MAX,
                        Next::PageEnd => unknown,
                    };
                    return Ok((None, next.max(unknown)));
                }
                // The index window of a page's first key is a prefix: when
                // another key shares it, the floor may be a neighbour.
                Look::BeforeFirst => {
                    if c == 0 || fwd {
                        return Ok((None, unknown));
                    }
                    c -= 1;
                    back = true;
                }
                Look::AfterLast => {
                    if back {
                        return Ok((None, unknown));
                    }
                    if !f.index.may_continue(c, key) {
                        // The next page starts with another key, unless it
                        // opens an index block with this very key, above `h`.
                        let next = f.index.starts_next_block(c, key).unwrap_or(u64::MAX);
                        return Ok((None, next.max(unknown)));
                    }
                    c += 1;
                    fwd = true;
                }
            }
            M.extra_pages.increment(1);
        }
    }

    fn get(&self, storage: bool, key: &[u8], h: u64) -> Result<Read, Error> {
        let v = self.view.load();
        let kind = storage as usize;
        let mut next = u64::MAX;
        for mem in std::iter::once(&v.active).chain(v.frozen.as_ref()) {
            let r = mem.get(storage, key, h);
            next = next.min(r.next);
            if r.found.is_some() {
                M.memtable_hits.increment(1);
                return Ok(Read {
                    found: r.found,
                    next,
                });
            }
        }
        let base = v.base_index(kind, key).map(|i| &v.base[kind][i]);
        for s in v.segs[kind].iter().rev() {
            if base.is_some_and(|b| s.hi <= b.hi) {
                // This segment and the older ones are in the base file.
                break;
            }
            if h < s.lo {
                // Wholly above the read height: a version here ends the
                // answer's span.
                if s.bloom.may_contain(key) {
                    next = next.min(s.lo);
                }
                continue;
            }
            if !s.bloom.may_contain(key) {
                continue;
            }
            M.segment_probes.increment(1);
            let (found, n) = self.file_get(&s.file, key, h)?;
            next = next.min(n);
            if found.is_some() {
                M.segment_hits.increment(1);
                return Ok(Read { found, next });
            }
        }
        let Some(base) = base else {
            return Ok(Read { found: None, next });
        };
        let (found, n) = self.file_get(&base.file, key, h)?;
        next = next.min(n);
        Ok(Read { found, next })
    }

    // ---- writes ----

    /// Move the memtable aside to be flushed, if it has anything.
    fn freeze(&self) -> bool {
        let st = self.state.lock().unwrap();
        let v = self.view.load_full();
        if v.frozen.is_some() || v.active.is_empty() {
            return false;
        }
        let mut nv = (*v).clone();
        nv.frozen = Some(std::mem::take(&mut nv.active));
        self.view.store(Arc::new(nv));
        drop(st);
        true
    }

    /// Write the frozen memtable as one segment per kind and publish them.
    fn flush_frozen(&self) -> Result<bool, Error> {
        let v = self.view.load_full();
        let Some(mem) = v.frozen.clone() else {
            return Ok(false);
        };
        let started = Instant::now();
        // The segments cover the blocks applied to the memtable before it
        // was frozen, with or without versions; nothing was applied to it
        // since.
        let (lo, hi) = mem.span().expect("a frozen memtable has blocks");
        let write = |storage: bool| -> Result<Option<Arc<Seg>>, Error> {
            let kind = storage as usize;
            let distinct = mem.keys(storage) as u64;
            if distinct == 0 {
                return Ok(None);
            }
            let dict = self.segment_dict(kind, &mem)?;
            let fid = self.new_fid();
            let name = format!("seg-{fid:08}-{}", kind_tag(kind));
            let path = self.dir.join(&name);
            let written = (|| -> Result<Arc<Seg>, Error> {
                let mut w = PageWriter::new(&path, storage, &dict, SEGMENT_LEVEL, Some(distinct))?;
                mem.for_each_sorted(storage, |k, b, v| w.add(k, b, v))?;
                let out = w.finish()?;
                Ok(Arc::new(Seg {
                    file: Arc::new(PageFile::open(&path, fid, storage, dict.clone())?),
                    name: name.clone(),
                    lo,
                    hi,
                    level: 0,
                    records: out.records,
                    distinct: out.distinct,
                    bloom: out.bloom.expect("asked for"),
                }))
            })();
            if written.is_err() {
                remove_page_file(&path);
            }
            written.map(Some)
        };
        let (a, s) = std::thread::scope(|scope| {
            let a = scope.spawn(|| write(false));
            let s = write(true);
            (a.join().expect("segment writer panicked"), s)
        });
        let new: Vec<Arc<Seg>> = [a?, s?].into_iter().flatten().collect();
        // Every block after the last one the block files hold, up to the
        // memtable's last: a block without an info is written as absent.
        let first = self.blocks.meta().last().map_or(lo, |last| last + 1);
        let mut recs: Vec<BlockRec> = Vec::new();
        let absent: BlockRec = ([0u8; 32], Arc::from(&[][..]));
        for (number, rec) in mem.blocks_in(first, hi + 1) {
            recs.resize((number - first) as usize, absent.clone());
            recs.push(rec);
        }
        if first <= hi {
            recs.resize((hi + 1 - first) as usize, absent);
        }
        // The first blocks a store gets choose the dictionary of its block
        // chunks, and the codes it holds, once they are many, that of its
        // code.
        {
            let infos: Vec<&[u8]> = recs.iter().map(|rec| &rec.1[..]).collect();
            self.adopt_dicts(self.blocks.train(&infos)?, self.codes.train_from_stored()?)?;
        }
        let blocks = self.blocks.append(first, &recs, side::LEVEL_SLOW)?;
        let codes = mem.codes();
        let codes = self.codes.append(
            codes.iter().map(|(hash, code)| (hash, &code[..])),
            side::LEVEL_SLOW,
        )?;
        sync_dir(&self.dir)?;
        let versions = mem.versions();
        {
            let mut st = self.state.lock().unwrap();
            let mut nv = (*self.view.load_full()).clone();
            for seg in new {
                nv.segs[seg.file.storage as usize].push(seg);
            }
            nv.frozen = None;
            nv.blocks = blocks.meta;
            nv.codes = codes.meta;
            self.publish(&mut st, nv, Some((blocks, codes)))?;
        }
        *self.merge_lock.lock().unwrap() = true;
        self.merge_cv.notify_all();
        M.flushes.increment(1);
        info!(
            target: "archive_v4",
            "flushed blocks {lo}..={hi}: {versions} versions in {:.1}s",
            started.elapsed().as_secs_f64()
        );
        let _g = self.flush_lock.lock().unwrap();
        self.flush_cv.notify_all();
        Ok(true)
    }

    /// Record the dictionaries just trained for the block and code files.
    fn adopt_dicts(
        &self,
        blocks: Option<BlocksAppend>,
        codes: Option<CodesAppend>,
    ) -> Result<(), Error> {
        if blocks.is_none() && codes.is_none() {
            return Ok(());
        }
        let blocks = match blocks {
            Some(dict) => dict,
            None => self.blocks.append(0, &[], side::LEVEL)?,
        };
        let codes = match codes {
            Some(dict) => dict,
            None => self.codes.append([], side::LEVEL)?,
        };
        let mut st = self.state.lock().unwrap();
        let mut nv = (*self.view.load_full()).clone();
        nv.blocks = blocks.meta;
        nv.codes = codes.meta;
        self.publish(&mut st, nv, Some((blocks, codes)))
    }

    /// The dictionary for a new segment of `kind`: the store's, or one
    /// trained on this memtable if it has none yet and there is enough data.
    fn segment_dict(&self, kind: usize, mem: &Mem) -> Result<Arc<Dict>, Error> {
        {
            let st = self.state.lock().unwrap();
            if st.segdict[kind] != NO_DICT {
                return Ok(st.dicts[&st.segdict[kind]].clone());
            }
        }
        let storage = kind == 1;
        let mut recs: Vec<Rec> = Vec::new();
        let k = key_len(storage);
        // (Sorted iteration is the only one a memtable has; the first flush
        // of a store walks it twice.)
        mem.for_each_sorted(storage, |key, block, val| {
            if recs.len() < 400_000 {
                let mut kk = [0u8; 64];
                kk[..k].copy_from_slice(key);
                recs.push(Rec {
                    key: kk,
                    block,
                    val: *val,
                });
            }
            Ok(())
        })?;
        let raw = file::train_dict(storage, recs.chunks(600).step_by(2));
        let mut st = self.state.lock().unwrap();
        let dict = self.add_dict(&mut st, raw)?;
        // Recorded in the manifest with the segment that uses it.
        st.segdict[kind] = dict.id;
        Ok(dict)
    }

    /// One size-tiered merge: the oldest `tiers` adjacent segments of the
    /// lowest level that has that many become one segment of the next.
    fn merge_step(&self) -> Result<bool, Error> {
        let _one = self.merging.lock().unwrap();
        let v = self.view.load_full();
        let pass = self.state.lock().unwrap().pass;
        for kind in 0..2 {
            let list = &v.segs[kind];
            let mut levels: Vec<u32> = list.iter().map(|s| s.level).collect();
            levels.sort_unstable();
            levels.dedup();
            for level in levels {
                // Segments are ordered by time; only neighbours merge, or
                // the result would overlap what lies between them. While a
                // base rewrite pass is under way, not across the block it
                // folds up to: the pass ends by dropping the segments up to
                // there, and one merged with later ones would stay, with
                // most of the delta in it.
                let Some(at) = (0..list.len()).find(|&i| {
                    i + self.opts.tiers <= list.len() && {
                        let group = &list[i..i + self.opts.tiers];
                        let target = pass[kind];
                        group.iter().all(|s| s.level == level)
                            && (target == 0
                                || group.iter().all(|s| s.hi <= target)
                                || group.iter().all(|s| s.hi > target))
                    }
                }) else {
                    continue;
                };
                let picked: Vec<Arc<Seg>> = list[at..at + self.opts.tiers].to_vec();
                self.merge_segments(kind, &picked, level + 1)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn merge_segments(&self, kind: usize, picked: &[Arc<Seg>], level: u32) -> Result<(), Error> {
        let started = Instant::now();
        let storage = kind == 1;
        let k = key_len(storage);
        let dict = {
            let st = self.state.lock().unwrap();
            st.dicts[&st.segdict[kind]].clone()
        };
        let fid = self.new_fid();
        let name = format!("seg-{fid:08}-{}", kind_tag(kind));
        let path = self.dir.join(&name);
        let cap: u64 = picked.iter().map(|s| s.distinct).sum();
        let result = (|| -> Result<Arc<Seg>, Error> {
            let mut w = PageWriter::new(&path, storage, &dict, SEGMENT_LEVEL, Some(cap))?;
            let inputs = picked
                .iter()
                .map(|s| Cursor::new(s.file.clone(), None, None, 0, u64::MAX))
                .collect();
            let mut it = MergeIter::new(inputs)?;
            let mut throttle = Throttle::new(self);
            let (mut n, mut spent) = (0u64, 0u64);
            while let Some(r) = it.next()? {
                w.add(&r.key[..k], r.block, &r.val)?;
                n += 1;
                if n % 4096 == 0 {
                    // Also where a closing store is noticed.
                    let now = it.io_bytes() + w.bytes();
                    throttle.spend(now - spent)?;
                    spent = now;
                }
            }
            let out = w.finish()?;
            sync_dir(&self.dir)?;
            Ok(Arc::new(Seg {
                file: Arc::new(PageFile::open(&path, fid, storage, dict.clone())?),
                name: name.clone(),
                lo: picked.iter().map(|s| s.lo).min().unwrap(),
                hi: picked.iter().map(|s| s.hi).max().unwrap(),
                level,
                records: out.records,
                distinct: out.distinct,
                bloom: out.bloom.expect("asked for"),
            }))
        })();
        let seg = match result {
            Ok(seg) => seg,
            Err(e) => {
                remove_page_file(&path);
                return Err(e);
            }
        };
        let in_bytes: u64 = picked.iter().map(|s| s.file.data_len()).sum();
        let (records, out_bytes) = (seg.records, seg.file.data_len());
        {
            let mut st = self.state.lock().unwrap();
            let mut nv = (*self.view.load_full()).clone();
            let ids: Vec<u32> = picked.iter().map(|s| s.file.fid).collect();
            // A base rewrite may have dropped some of the inputs meanwhile;
            // the merged segment then stands for those that are left.
            let Some(at) = nv.segs[kind].iter().position(|s| ids.contains(&s.file.fid)) else {
                drop(st);
                remove_page_file(&path);
                return Ok(());
            };
            nv.segs[kind].retain(|s| !ids.contains(&s.file.fid));
            nv.segs[kind].insert(at, seg);
            self.publish(&mut st, nv, None)?;
        }
        for s in picked {
            remove_page_file(&self.dir.join(&s.name));
        }
        M.merges.increment(1);
        info!(
            target: "archive_v4",
            "merged {} {} segments into level {level}: {records} versions, {:.2} -> {:.2} GiB, {:.1}s",
            picked.len(),
            if storage { "storage" } else { "account" },
            in_bytes as f64 / (1u64 << 30) as f64,
            out_bytes as f64 / (1u64 << 30) as f64,
            started.elapsed().as_secs_f64()
        );
        Ok(())
    }

    /// Fold the delta into the base range of `old` (`None`: this kind has
    /// no base yet, the range is everything). Pages no delta version falls
    /// into are copied as stored. Returns the number of files written.
    fn rewrite_range(
        &self,
        kind: usize,
        old: Option<&Arc<BaseFile>>,
        throttle: &mut Throttle,
    ) -> Result<usize, Error> {
        let started = Instant::now();
        let storage = kind == 1;
        let k = key_len(storage);
        let v = self.view.load_full();
        // The range of the file, by the files around it now.
        let (lo, hi) = match old {
            None => (None, None),
            Some(old) => {
                let Some(i) = v.base[kind].iter().position(|b| b.file.fid == old.file.fid) else {
                    return Ok(0);
                };
                (
                    (i > 0).then_some(old.first_key),
                    v.base[kind].get(i + 1).map(|b| b.first_key),
                )
            }
        };
        let from = old.map_or(0, |o| o.hi + 1);
        let segs: Vec<Arc<Seg>> = v.segs[kind]
            .iter()
            .filter(|s| s.hi >= from)
            .cloned()
            .collect();
        let Some(new_hi) = segs.iter().map(|s| s.hi).max() else {
            return Ok(0);
        };
        // The rewrite of a file takes long: it keeps its inputs, not the
        // memtables and every other file of this moment.
        drop(v);
        let dict = match old {
            // The old file's dictionary, so that its pages can be copied.
            Some(old) => old.file.dict.clone(),
            None => {
                let st = self.state.lock().unwrap();
                st.dicts[&st.segdict[kind]].clone()
            }
        };
        let mut delta = MergeIter::new(
            segs.iter()
                .map(|s| Cursor::new(s.file.clone(), lo, hi, from, u64::MAX))
                .collect(),
        )?;
        let mut out = BaseOut::new(self, storage, dict, new_hi);
        let mut stats = (0u64, 0u64, 0u64); // pages copied, pages merged, delta versions
        let result = (|| -> Result<(), Error> {
            let lt = |a: &([u8; 64], u64), key: &[u8], block: u64| (&a.0[..k], a.1) < (key, block);
            let mut spent = 0u64;
            let Some(old) = old else {
                while let Some(r) = delta.next()? {
                    out.add(&r.key[..k], r.block, &r.val)?;
                    stats.2 += 1;
                    if stats.2 % 4096 == 0 {
                        let now = delta.io_bytes() + out.bytes();
                        throttle.spend(now - spent)?;
                        spent = now;
                    }
                }
                return Ok(());
            };
            let mut src = SeqReader::new(old.file.clone());
            let n = old.file.index.pages();
            let mut recs: Vec<Rec> = Vec::new();
            // The page being placed and the one after it, whose first record
            // bounds the delta versions that fall into this one.
            let (mut cur, mut ahead) = (Ahead::default(), Ahead::default());
            if n > 0 {
                cur.load(&mut src, 0, storage)?;
                // Delta versions before the first base page.
                while delta
                    .peek()
                    .is_some_and(|d| lt(&d, &cur.key[..k], cur.block))
                {
                    let r = delta.next()?.expect("peeked");
                    out.add(&r.key[..k], r.block, &r.val)?;
                    stats.2 += 1;
                }
            }
            for c in 0..n {
                let bound = if c + 1 < n {
                    ahead.load(&mut src, c + 1, storage)?;
                    Some((*ahead.key, ahead.block))
                } else {
                    None
                };
                let touched = match (&bound, delta.peek()) {
                    (_, None) => false,
                    (None, Some(_)) => true,
                    (Some((nk, nb)), Some(d)) => lt(&d, &nk[..k], *nb),
                };
                // A short open page absorbs this one instead of being cut
                // short; a file past its size needs records to find where
                // it may end.
                if !touched && !out.open_page_is_short() && !out.wants_cut() {
                    out.copy_page(&cur.key[..k], cur.block, &cur.stored, cur.records)?;
                    stats.0 += 1;
                } else {
                    stats.1 += 1;
                    recs.clear();
                    codec::for_each(&cur.raw, storage, |key, block, val| {
                        let mut kk = [0u8; 64];
                        kk[..k].copy_from_slice(key);
                        recs.push(Rec {
                            key: kk,
                            block,
                            val,
                        });
                    })?;
                    let mut i = 0usize;
                    loop {
                        let d = delta
                            .peek()
                            .filter(|d| bound.as_ref().is_none_or(|b| lt(d, &b.0[..k], b.1)));
                        let take_base = match (recs.get(i), d) {
                            (None, None) => break,
                            (Some(_), None) => true,
                            (None, Some(_)) => false,
                            (Some(r), Some(d)) => (&r.key[..k], r.block) < (&d.0[..k], d.1),
                        };
                        if take_base {
                            let r = &recs[i];
                            out.add(&r.key[..k], r.block, &r.val)?;
                            i += 1;
                        } else {
                            let r = delta.next()?.expect("peeked");
                            out.add(&r.key[..k], r.block, &r.val)?;
                            stats.2 += 1;
                        }
                    }
                }
                std::mem::swap(&mut cur, &mut ahead);
                let now = src.io_bytes + delta.io_bytes() + out.bytes();
                throttle.spend(now - spent)?;
                spent = now;
            }
            while let Some(r) = delta.next()? {
                out.add(&r.key[..k], r.block, &r.val)?;
                stats.2 += 1;
            }
            Ok(())
        })();
        if let Err(e) = result {
            out.abort();
            return Err(e);
        }
        let out_bytes = out.bytes();
        let files = out.finish()?;
        let n_files = files.len();
        let mut dropped_segs = Vec::new();
        // Files that do not get into the manifest are not left behind.
        let unlisted: Vec<String> = files.iter().map(|b| b.name.clone()).collect();
        let discard = |e: Error| {
            for name in &unlisted {
                remove_page_file(&self.dir.join(name));
            }
            e
        };
        sync_dir(&self.dir).map_err(discard)?;
        {
            let mut st = self.state.lock().unwrap();
            let mut nv = (*self.view.load_full()).clone();
            match old {
                Some(old) => {
                    let at = nv.base[kind]
                        .iter()
                        .position(|b| b.file.fid == old.file.fid)
                        .ok_or_else(|| discard(corrupt("base file vanished during its rewrite")))?;
                    nv.base[kind].splice(at..at + 1, files);
                }
                None => nv.base[kind] = files,
            }
            // Segments every base file of the kind now holds are done.
            if let Some(min_hi) = nv.base[kind].iter().map(|b| b.hi).min() {
                nv.segs[kind].retain(|s| {
                    let keep = s.hi > min_hi;
                    if !keep {
                        dropped_segs.push(s.clone());
                    }
                    keep
                });
            }
            // A manifest that failed to be written may still be the one the
            // next open finds: the new files stay for it to sort out.
            self.publish(&mut st, nv, None)?;
        }
        if let Some(old) = old {
            remove_page_file(&self.dir.join(&old.name));
        }
        for s in &dropped_segs {
            remove_page_file(&self.dir.join(&s.name));
        }
        M.rewrites.increment(1);
        M.rewrite_bytes.increment(out_bytes);
        info!(
            target: "archive_v4",
            "rewrote {} base {}: through block {new_hi}, {} pages copied, {} merged, {} delta versions, {:.2} -> {:.2} GiB in {n_files} files, {:.0}s",
            if storage { "storage" } else { "account" },
            old.map_or("(new)".to_string(), |o| o.name.clone()),
            stats.0,
            stats.1,
            stats.2,
            old.map_or(0, |o| o.file.data_len()) as f64 / (1u64 << 30) as f64,
            out_bytes as f64 / (1u64 << 30) as f64,
            started.elapsed().as_secs_f64()
        );
        Ok(n_files)
    }

    /// Fold every segment now present into the base, file by file. The
    /// block a pass folds up to is recorded when it starts: a pass that was
    /// cut short (the node stopped) is taken up with that block, and leaves
    /// the files it had rewritten alone. Without it they would all be
    /// behind the segments flushed since, and a node that restarts more
    /// often than a pass takes would never finish one.
    fn rewrite_pass(&self) -> Result<(), Error> {
        let _one = self.rewriting.lock().unwrap();
        let mut throttle = Throttle::new(self);
        // The block each kind is folded up to; 0: nothing to do there.
        let mut targets = [0u64; 2];
        for (kind, target) in targets.iter_mut().enumerate() {
            let v = self.view.load_full();
            let Some(newest) = v.segs[kind].iter().map(|s| s.hi).max() else {
                self.set_pass(kind, 0)?;
                continue;
            };
            if v.base[kind].is_empty() {
                self.rewrite_range(kind, None, &mut throttle)?;
                continue;
            }
            let recorded = self.state.lock().unwrap().pass[kind];
            // A recorded block above every segment is from before a rewind.
            *target = if recorded != 0 && recorded <= newest {
                recorded
            } else {
                self.set_pass(kind, newest)?;
                newest
            };
        }
        // The file furthest behind first, of either kind: a segment is
        // dropped when every file of its kind holds it, so the files behind
        // are what the oldest segments wait for. One pass after another
        // then goes round the base in the same order, and drops segments as
        // it goes.
        let mut stuck: HashSet<u32> = HashSet::new();
        loop {
            let v = self.view.load_full();
            let next = (0..2)
                .flat_map(|kind| v.base[kind].iter().map(move |b| (kind, b)))
                .filter(|(kind, b)| b.hi < targets[*kind] && !stuck.contains(&b.file.fid))
                .min_by_key(|(kind, b)| (b.hi, *kind));
            let Some((kind, b)) = next.map(|(kind, b)| (kind, b.clone())) else {
                break;
            };
            drop(v);
            if self.rewrite_range(kind, Some(&b), &mut throttle)? == 0 {
                // Nothing to fold into it after all.
                stuck.insert(b.file.fid);
            }
        }
        for kind in 0..2 {
            self.set_pass(kind, 0)?;
        }
        Ok(())
    }

    /// Record that a rewind to `block` is under way, or (`None`) over.
    fn set_rewinding(&self, block: Option<u64>) -> Result<(), Error> {
        let mut st = self.state.lock().unwrap();
        if st.rewinding == block {
            return Ok(());
        }
        st.rewinding = block;
        let v = (*self.view.load_full()).clone();
        self.publish(&mut st, v, None)
    }

    /// Segment `seg` without its versions after `block`, as a new segment;
    /// `None` if none is left.
    fn trim_segment(
        &self,
        kind: usize,
        seg: &Arc<Seg>,
        block: u64,
    ) -> Result<Option<Arc<Seg>>, Error> {
        let storage = kind == 1;
        let k = key_len(storage);
        let dict = seg.file.dict.clone();
        let fid = self.new_fid();
        let name = format!("seg-{fid:08}-{}", kind_tag(kind));
        let path = self.dir.join(&name);
        let written = (|| -> Result<Option<Arc<Seg>>, Error> {
            let mut w = PageWriter::new(&path, storage, &dict, SEGMENT_LEVEL, Some(seg.distinct))?;
            let mut cursor = Cursor::new(seg.file.clone(), None, None, 0, block);
            while let Some(r) = cursor.next()? {
                w.add(&r.key[..k], r.block, &r.val)?;
            }
            let out = w.finish()?;
            if out.records == 0 {
                return Ok(None);
            }
            sync_dir(&self.dir)?;
            Ok(Some(Arc::new(Seg {
                file: Arc::new(PageFile::open(&path, fid, storage, dict.clone())?),
                name: name.clone(),
                lo: seg.lo,
                hi: block,
                level: seg.level,
                records: out.records,
                distinct: out.distinct,
                bloom: out.bloom.expect("asked for"),
            })))
        })();
        if !matches!(written, Ok(Some(_))) {
            remove_page_file(&path);
        }
        written
    }

    /// Base file `old` without its versions after `block`, as new files
    /// that hold the history up to `block`.
    fn trim_base(
        &self,
        kind: usize,
        old: &Arc<BaseFile>,
        block: u64,
    ) -> Result<Vec<Arc<BaseFile>>, Error> {
        let storage = kind == 1;
        let k = key_len(storage);
        let mut out = BaseOut::new(self, storage, old.file.dict.clone(), block);
        let mut cursor = Cursor::new(old.file.clone(), None, None, 0, block);
        let result = (|| -> Result<(), Error> {
            while let Some(r) = cursor.next()? {
                out.add(&r.key[..k], r.block, &r.val)?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            out.abort();
            return Err(e);
        }
        let files = out.finish()?;
        sync_dir(&self.dir)?;
        Ok(files)
    }

    /// Record the block the rewrite pass of `kind` folds up to (0: none).
    fn set_pass(&self, kind: usize, target: u64) -> Result<(), Error> {
        let mut st = self.state.lock().unwrap();
        if st.pass[kind] == target {
            return Ok(());
        }
        st.pass[kind] = target;
        let v = (*self.view.load_full()).clone();
        self.publish(&mut st, v, None)
    }

    /// Whether the delta has grown enough for a base rewrite.
    fn rewrite_due(&self) -> bool {
        let v = self.view.load();
        let delta: u64 = (0..2)
            .flat_map(|k| v.delta(k))
            .map(|s| s.file.data_len())
            .sum();
        if delta == 0 {
            return false;
        }
        let base: u64 = v.base.iter().flatten().map(|b| b.file.data_len()).sum();
        let filters: u64 = v.segs.iter().flatten().map(|s| s.bloom.mem_bytes()).sum();
        (delta >= self.opts.delta_min_bytes && delta as f64 >= base as f64 * self.opts.delta_ratio)
            || filters >= self.opts.delta_filter_bytes
    }

    // ---- background threads ----

    fn fail(&self, what: &str, e: Error) {
        if self.stop.load(Ordering::Relaxed) {
            return;
        }
        error!(target: "archive_v4", "{what} failed: {e}");
        *self.failed.lock().unwrap() = Some(format!("{what}: {e}"));
        let _g = self.flush_lock.lock().unwrap();
        self.flush_cv.notify_all();
    }

    /// Flushes frozen memtables and merges segments.
    fn flusher(&self) {
        while !self.stop.load(Ordering::Relaxed) {
            match self.flush_frozen() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => return self.fail("memtable flush", e),
            }
            M.page_cache_usage_bytes.set(self.cache.weight() as f64);
            let g = self.flush_lock.lock().unwrap();
            if self.view.load().frozen.is_none() && !self.stop.load(Ordering::Relaxed) {
                let _ = self.flush_cv.wait_timeout(g, Duration::from_secs(5));
            }
        }
    }

    /// Merges segments. Not on the flusher's thread: a merge is paced and
    /// may take long, and a memtable must not wait for it to be written.
    fn merger(&self) {
        while !self.stop.load(Ordering::Relaxed) {
            match self.merge_step() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    // A merge can be retried; a failed one leaves the
                    // segments as they were.
                    if !self.stop.load(Ordering::Relaxed) {
                        warn!(target: "archive_v4", "segment merge failed: {e}");
                    }
                }
            }
            let mut woken = self.merge_lock.lock().unwrap();
            if !*woken && !self.stop.load(Ordering::Relaxed) {
                woken = self
                    .merge_cv
                    .wait_timeout(woken, Duration::from_secs(5))
                    .unwrap()
                    .0;
            }
            *woken = false;
        }
    }

    /// Rewrites the base.
    fn rewriter(&self) {
        while !self.stop.load(Ordering::Relaxed) {
            if self.rewrite_due() {
                if let Err(e) = self.rewrite_pass() {
                    if !self.stop.load(Ordering::Relaxed) {
                        warn!(target: "archive_v4", "base rewrite failed, will retry: {e}");
                    }
                }
            }
            let mut woken = self.rewrite_lock.lock().unwrap();
            if !*woken && !self.stop.load(Ordering::Relaxed) {
                woken = self
                    .rewrite_cv
                    .wait_timeout(woken, self.opts.rewrite_check)
                    .unwrap()
                    .0;
            }
            *woken = false;
        }
    }
}

/// Memory the index of every file in `v` takes.
fn index_bytes(v: &View) -> u64 {
    v.base
        .iter()
        .flatten()
        .map(|b| b.file.index.mem_bytes())
        .chain(v.segs.iter().flatten().map(|s| s.file.index.mem_bytes()))
        .sum()
}

impl Store {
    /// Whether `dir` holds a store.
    pub(crate) fn exists(dir: &Path) -> bool {
        dir.join(MANIFEST).is_file()
    }

    /// Lay out an empty store at `dir`.
    pub(crate) fn create(dir: &Path) -> Result<(), Error> {
        std::fs::create_dir_all(dir)?;
        if Self::exists(dir) {
            return Ok(());
        }
        side::create(dir)?;
        ManifestData::empty().write(dir)
    }

    /// Lay out a store at `dir` over base files, and block and code files,
    /// already written there (an offline conversion). The base files of a
    /// kind must cover disjoint, ascending key ranges.
    pub(crate) fn create_with_base(
        dir: &Path,
        files: Vec<NewBase>,
        blocks: BlocksMeta,
        codes: CodesMeta,
    ) -> Result<(), Error> {
        let mut m = ManifestData::empty();
        m.blocks = blocks;
        m.codes = codes;
        let mut dict_ids: HashMap<String, u32> = HashMap::new();
        for f in files {
            let dict = match &f.dict {
                None => NO_DICT,
                Some(name) => match dict_ids.get(name) {
                    Some(id) => *id,
                    None => {
                        let id = m.next_dict;
                        m.next_dict += 1;
                        // Under the store's own name for it.
                        std::fs::rename(dir.join(name), dir.join(dict_file(id)))?;
                        m.dicts.push((id, dict_file(id)));
                        dict_ids.insert(name.clone(), id);
                        id
                    }
                },
            };
            let kind = f.storage as usize;
            // New segments use the dictionary of the kind's first base file.
            if m.segdict[kind] == NO_DICT {
                m.segdict[kind] = dict;
            }
            m.base
                .push((kind, f.fid, dict, f.hi, f.records, f.name, f.first_key));
            m.next_fid = m.next_fid.max(f.fid + 1);
        }
        sync_dir(dir)?;
        m.write(dir)
    }

    pub(crate) fn open(dir: &Path, opts: Options) -> Result<Self, Error> {
        let started = Instant::now();
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LOCK))?;
        {
            use std::os::fd::AsRawFd;
            // SAFETY: `flock` on a descriptor this function owns.
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(Error::UnSupported(format!(
                    "the archive v4 store {} is open in another process",
                    dir.display()
                )));
            }
        }
        let m = ManifestData::parse(&std::fs::read_to_string(dir.join(MANIFEST))?)?;
        if let Some(block) = m.rewinding {
            // Some files hold the history up to that block, others beyond.
            if opts.background {
                return Err(Error::UnSupported(format!(
                    "a rewind of this archive to block {block} was cut short: run it again \
                     (to that block or below) before the archive is used"
                )));
            }
            warn!(target: "archive_v4", "a rewind to block {block} was cut short and is to be run again");
        }
        let mut dicts: HashMap<u32, Arc<Dict>> = HashMap::new();
        dicts.insert(NO_DICT, Arc::new(Dict::new(NO_DICT, Vec::new())));
        for (id, name) in &m.dicts {
            dicts.insert(
                *id,
                Arc::new(Dict::new(*id, std::fs::read(dir.join(name))?)),
            );
        }
        let dict = |id: u32| -> Result<Arc<Dict>, Error> {
            dicts
                .get(&id)
                .cloned()
                .ok_or_else(|| corrupt(format!("manifest names unknown dictionary {id}")))
        };
        // Leftovers of an interrupted flush, merge or rewrite.
        let mut known: HashSet<String> = [MANIFEST.to_string()].into();
        known.extend(m.dicts.iter().map(|d| d.1.clone()));
        for name in m
            .base
            .iter()
            .map(|b| &b.5)
            .chain(m.segs.iter().map(|s| &s.8))
        {
            known.extend(["v4", "cidx", "bloom"].map(|ext| format!("{name}.{ext}")));
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let ours = ["seg-", "base-", "flt-", "dict-"]
                .iter()
                .any(|p| name.starts_with(p))
                || name.ends_with(".tmp");
            if ours && !known.contains(&name) {
                warn!(target: "archive_v4", "removing leftover file {name}");
                let _ = std::fs::remove_file(entry.path());
            }
        }

        // Indexes are read in parallel: a large archive has gigabytes.
        type Job<'a> = Box<dyn FnOnce() -> Result<(), Error> + Send + 'a>;
        let base_slots: Vec<Mutex<Option<BaseFile>>> =
            m.base.iter().map(|_| Mutex::new(None)).collect();
        let seg_slots: Vec<Mutex<Option<Seg>>> = m.segs.iter().map(|_| Mutex::new(None)).collect();
        let mut jobs: Vec<Job> = Vec::new();
        for (i, (kind, fid, d, hi, records, name, first_key)) in m.base.iter().enumerate() {
            let (slot, d) = (&base_slots[i], dict(*d)?);
            jobs.push(Box::new(move || {
                let file = PageFile::open(&dir.join(name), *fid, *kind == 1, d)?;
                *slot.lock().unwrap() = Some(BaseFile {
                    file: Arc::new(file),
                    name: name.clone(),
                    hi: *hi,
                    records: *records,
                    first_key: *first_key,
                });
                Ok(())
            }));
        }
        for (i, (kind, fid, d, lo, hi, level, records, distinct, name)) in m.segs.iter().enumerate()
        {
            let (slot, d) = (&seg_slots[i], dict(*d)?);
            jobs.push(Box::new(move || {
                let base = dir.join(name);
                let file = PageFile::open(&base, *fid, *kind == 1, d)?;
                *slot.lock().unwrap() = Some(Seg {
                    file: Arc::new(file),
                    name: name.clone(),
                    lo: *lo,
                    hi: *hi,
                    level: *level,
                    records: *records,
                    distinct: *distinct,
                    bloom: Bloom::load(&bloom_path(&base))?,
                });
                Ok(())
            }));
        }
        let jobs = Mutex::new(jobs);
        let errors: Mutex<Vec<Error>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| loop {
                    let Some(job) = jobs.lock().unwrap().pop() else {
                        break;
                    };
                    if let Err(e) = job() {
                        errors.lock().unwrap().push(e);
                    }
                });
            }
        });
        drop(jobs);
        if let Some(e) = errors.into_inner().unwrap().pop() {
            return Err(e);
        }
        let mut view = View {
            active: Arc::new(Mem::default()),
            frozen: None,
            segs: [Vec::new(), Vec::new()],
            base: [Vec::new(), Vec::new()],
            blocks: m.blocks,
            codes: m.codes,
        };
        let blocks = Blocks::open(dir, m.blocks, opts.block_cache_bytes)?;
        let codes = Codes::open(dir, m.codes)?;
        for (slot, entry) in base_slots.into_iter().zip(&m.base) {
            let b = slot.into_inner().unwrap().expect("opened");
            view.base[entry.0].push(Arc::new(b));
        }
        for (slot, entry) in seg_slots.into_iter().zip(&m.segs) {
            let s = slot.into_inner().unwrap().expect("opened");
            view.segs[entry.0].push(Arc::new(s));
        }
        for kind in 0..2 {
            let k = key_len(kind == 1);
            view.base[kind].sort_by(|a, b| a.first_key.cmp(&b.first_key));
            if view.base[kind]
                .windows(2)
                .any(|w| w[0].first_key[..k] >= w[1].first_key[..k])
            {
                return Err(corrupt("base files overlap"));
            }
            view.segs[kind].sort_by_key(|s| s.lo);
            if view.segs[kind].windows(2).any(|w| w[0].hi >= w[1].lo) {
                return Err(corrupt("segments overlap"));
            }
        }
        // What the index and filters hold now, and room for what the delta
        // may grow to, come out of the budget; the page cache gets the rest.
        let index = index_bytes(&view);
        let seg_filters: u64 = view
            .segs
            .iter()
            .flatten()
            .map(|s| s.bloom.mem_bytes())
            .sum();
        let side = blocks.mem_bytes() + codes.mem_bytes() + opts.block_cache_bytes;
        let reserved = index
            + side
            + seg_filters.max(if opts.delta_filter_bytes == u64::MAX {
                0
            } else {
                opts.delta_filter_bytes
            });
        let cache_bytes = opts.budget_bytes.saturating_sub(reserved).max(64 << 20);
        let next_block = view.durable().map_or(0, |d| d + 1);
        info!(
            target: "archive_v4",
            "opened {dir:?} in {:.1}s: {} base files, {} segments, {} blocks and {} codes, through block {:?}; index {:.2} GiB, segment filters {:.1} MiB, page cache {:.2} GiB",
            started.elapsed().as_secs_f64(),
            view.base.iter().flatten().count(),
            view.segs.iter().flatten().count(),
            view.blocks.count,
            view.codes.count,
            view.durable(),
            index as f64 / (1u64 << 30) as f64,
            seg_filters as f64 / (1u64 << 20) as f64,
            cache_bytes as f64 / (1u64 << 30) as f64,
        );
        let inner = Arc::new(Inner {
            dir: dir.to_path_buf(),
            cache: QuickCache::with_weighter(
                (cache_bytes / (codec::PAGE_TARGET as u64 + 1024)) as usize,
                cache_bytes,
                PageWeighter,
            ),
            view: ArcSwap::from_pointee(view),
            write: Mutex::new(()),
            state: Mutex::new(State {
                next_fid: m.next_fid,
                next_dict: m.next_dict,
                dicts,
                segdict: m.segdict,
                pass: m.pass,
                rewinding: m.rewinding,
            }),
            next_block: AtomicU64::new(next_block),
            flush_lock: Mutex::new(()),
            flush_cv: Condvar::new(),
            rewriting: Mutex::new(()),
            merging: Mutex::new(()),
            rewrite_lock: Mutex::new(false),
            merge_lock: Mutex::new(false),
            merge_cv: Condvar::new(),
            paced: Mutex::new(Instant::now()),
            rewrite_cv: Condvar::new(),
            stop: AtomicBool::new(false),
            failed: Mutex::new(None),
            blocks,
            codes,
            _lock: lock,
            opts,
        });
        inner.set_gauges(&inner.view.load());
        M.page_cache_capacity_bytes.set(cache_bytes as f64);
        let mut threads = Vec::new();
        if inner.opts.background {
            for (name, run) in [
                ("v4-flush", Inner::flusher as fn(&Inner)),
                ("v4-merge", Inner::merger),
                ("v4-rewrite", Inner::rewriter),
            ] {
                let inner = inner.clone();
                threads.push(
                    std::thread::Builder::new()
                        .name(name.into())
                        .spawn(move || run(&inner))?,
                );
            }
        }
        Ok(Self {
            inner,
            threads: Mutex::new(threads),
        })
    }

    /// The last block held by a file: blocks above it live in the memtable
    /// only and must be applied again after a restart.
    #[cfg(test)]
    pub(crate) fn durable(&self) -> Option<u64> {
        self.inner.view.load().durable()
    }

    /// The last block applied.
    pub(crate) fn head(&self) -> Option<u64> {
        self.inner.next_block.load(Ordering::Acquire).checked_sub(1)
    }

    /// The greatest version of `key` at or below `h`. An account key is the
    /// 32-byte address hash; a storage key is address hash ‖ slot hash.
    pub(crate) fn get(&self, storage: bool, key: &[u8], h: u64) -> Result<Read, Error> {
        debug_assert_eq!(key.len(), key_len(storage));
        self.inner.get(storage, key, h)
    }

    /// Add the versions of `block`: at most one per key. See
    /// [`Self::commit`].
    #[cfg(test)]
    pub(crate) fn apply(
        &self,
        block: u64,
        accounts: &[([u8; 32], Val)],
        slots: &[([u8; 64], [u8; 32])],
    ) -> Result<(), Error> {
        self.commit(&[(block, accounts, slots)], &[], &[])
    }

    /// Add blocks: their versions (at most one per key and block), their
    /// infos, and code first seen in them; `versions` and `blocks` in block
    /// order. Blocks come in order. The block applied last may be applied
    /// again, as the block it is: with the hash it has, and its versions
    /// are written over (a different block at that number needs a rewind);
    /// a block on its way into the files is only accepted again with the
    /// hash they have, and without versions. A block without versions only
    /// moves the head. Everything stays in memory until
    /// [`Self::flush_if_due`] or [`Self::flush`] writes it out.
    ///
    /// Readers do not wait for the writer. They take a block for applied
    /// when they find its info or see the head at it, so those come last:
    /// first the code (found by its hash), then per block its versions,
    /// its info, the head.
    pub(crate) fn commit(
        &self,
        versions: &[BlockVersions],
        blocks: &[(u64, BlockRec)],
        codes: &[([u8; 32], Arc<[u8]>)],
    ) -> Result<(), Error> {
        let inner = &*self.inner;
        inner.check_failed()?;
        let top = versions
            .iter()
            .map(|v| v.0)
            .chain(blocks.iter().map(|b| b.0))
            .max();
        if let Some(block) = top.filter(|b| *b > u32::MAX as u64) {
            return Err(Error::UnSupported(format!(
                "archive schema v4 holds block numbers up to 2^32, not {block}"
            )));
        }
        let _w = inner.write.lock().unwrap();
        let v = inner.view.load_full();
        // Blocks up to here are in the files, or in the memtable being
        // written to them.
        let fixed = v
            .frozen
            .as_ref()
            .and_then(|m| m.span())
            .map(|span| span.1)
            .max(v.durable());
        let is_fixed = |block: u64| fixed.is_some_and(|f| block <= f);
        let refuse = |block: u64| {
            Error::UnSupported(format!(
                "archive schema v4 already holds state up to block {}; writing block {block} \
                 needs a rewind to below it first (a head-only rewind is not supported)",
                inner.next_block.load(Ordering::Acquire).saturating_sub(1)
            ))
        };
        let mem = &v.active;
        for (hash, code) in codes {
            mem.put_code(*hash, code.clone());
        }
        let (mut vi, mut bi) = (0, 0);
        loop {
            let block = match (versions.get(vi), blocks.get(bi)) {
                (Some(v), Some(b)) => v.0.min(b.0),
                (Some(v), None) => v.0,
                (None, Some(b)) => b.0,
                (None, None) => break,
            };
            let next = inner.next_block.load(Ordering::Acquire);
            // The info to put, once the block's versions are in.
            let mut info = None;
            if let Some((_, rec)) = blocks.get(bi).filter(|b| b.0 == block) {
                bi += 1;
                if is_fixed(block) {
                    let held = v
                        .frozen
                        .as_ref()
                        .and_then(|m| m.block(block))
                        .map_or_else(|| inner.blocks.get(block), |rec| Ok(Some(rec)))?;
                    if held.map(|held| held.0) != Some(rec.0) {
                        return Err(refuse(block));
                    }
                } else if block + 1 < next || mem.block(block).is_some_and(|held| held.0 != rec.0) {
                    // Below the head, or another block in the head's place:
                    // the versions the first one wrote would stay.
                    return Err(refuse(block));
                } else {
                    info = Some(rec);
                }
            }
            if let Some((_, accounts, slots)) = versions.get(vi).filter(|v| v.0 == block) {
                vi += 1;
                if !accounts.is_empty() || !slots.is_empty() {
                    if block + 1 < next || is_fixed(block) {
                        return Err(refuse(block));
                    }
                    let mut key = [0u8; 64];
                    for (address, val) in accounts.iter() {
                        key[..32].copy_from_slice(address);
                        mem.insert(false, &key, block, val)?;
                    }
                    for (key, value) in slots.iter() {
                        mem.insert(true, key, block, &Val::Storage(*value))?;
                    }
                }
                if !is_fixed(block) && block + 1 >= next {
                    mem.note_block(block);
                }
            }
            if let Some(rec) = info {
                mem.put_block(block, rec.clone());
            }
            inner.next_block.fetch_max(block + 1, Ordering::AcqRel);
        }
        M.memtable_versions.set(mem.versions() as f64);
        Ok(())
    }

    /// The hash and info of block `number`.
    pub(crate) fn block(&self, number: u64) -> Result<Option<BlockRec>, Error> {
        let v = self.inner.view.load();
        match v
            .active
            .block(number)
            .or_else(|| v.frozen.as_ref().and_then(|m| m.block(number)))
        {
            Some(rec) => Ok(Some(rec)),
            None => self.inner.blocks.get(number),
        }
    }

    /// The hash of block `number`.
    pub(crate) fn block_hash(&self, number: u64) -> Result<Option<[u8; 32]>, Error> {
        let v = self.inner.view.load();
        match v
            .active
            .block(number)
            .or_else(|| v.frozen.as_ref().and_then(|m| m.block(number)))
        {
            Some(rec) => Ok(Some(rec.0)),
            None => Ok(self.inner.blocks.get(number)?.map(|rec| rec.0)),
        }
    }

    /// The first 8 bytes of the hash of each block in `[from, to)` that has
    /// one, in order.
    pub(crate) fn scan_block_prefixes(
        &self,
        from: u64,
        to: u64,
        f: &mut dyn FnMut(u64, u64) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let v = self.inner.view.load_full();
        // The files first, as the view saw them; then what its memtables
        // hold above them.
        let files_end = v.blocks.last().map_or(0, |last| last + 1);
        self.inner
            .blocks
            .scan_prefixes(from, to.min(files_end), &mut |number, prefix| {
                if prefix != 0 {
                    f(number, prefix)?;
                }
                Ok(())
            })?;
        let mut in_mem: Vec<(u64, BlockRec)> = Vec::new();
        for mem in v.frozen.iter().chain([&v.active]) {
            in_mem.extend(mem.blocks_in(from.max(files_end), to));
        }
        in_mem.sort_by_key(|b| b.0);
        in_mem.dedup_by_key(|b| b.0);
        for (number, rec) in in_mem {
            f(number, u64::from_be_bytes(rec.0[..8].try_into().unwrap()))?;
        }
        Ok(())
    }

    /// The code with hash `hash`.
    pub(crate) fn code(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, Error> {
        let v = self.inner.view.load();
        match v
            .active
            .code(hash)
            .or_else(|| v.frozen.as_ref().and_then(|m| m.code(hash)))
        {
            Some(code) => Ok(Some(code.to_vec())),
            None => self.inner.codes.get(hash),
        }
    }

    /// Start writing the memtable to a segment if it is large or old
    /// enough. Called once the caller's own record of the applied blocks is
    /// written, so that a segment never holds a block the caller lost.
    pub(crate) fn flush_if_due(&self) -> Result<(), Error> {
        let inner = &*self.inner;
        let _w = inner.write.lock().unwrap();
        {
            let v = inner.view.load();
            let mem = &v.active;
            if mem.is_empty() {
                return Ok(());
            }
            let Some(head) = self.head() else {
                return Ok(());
            };
            // On the blocks spanned, or on size (looked at now and then).
            let spans = head.saturating_sub(mem.span().map_or(head, |span| span.0)) + 1;
            if spans < inner.opts.flush_blocks
                && (head % 16 != 0 || mem.bytes() < inner.opts.flush_bytes)
            {
                return Ok(());
            }
        }
        if !inner.opts.background {
            if inner.freeze() {
                inner.flush_frozen()?;
            }
            return Ok(());
        }
        // The flusher takes it from here; when it is still busy with the
        // previous memtable, wait, so that memory stays bounded.
        let mut g = inner.flush_lock.lock().unwrap();
        loop {
            inner.check_failed()?;
            if inner.freeze() {
                inner.flush_cv.notify_all();
                return Ok(());
            }
            if inner.view.load().active.is_empty() {
                return Ok(());
            }
            g = inner
                .flush_cv
                .wait_timeout(g, Duration::from_millis(200))
                .unwrap()
                .0;
        }
    }

    /// The slots of `address` holding a non-zero value at `h`.
    pub(crate) fn storage_slots(
        &self,
        address: &[u8; 32],
        h: u64,
    ) -> Result<Vec<([u8; 32], [u8; 32])>, Error> {
        let inner = &*self.inner;
        let v = inner.view.load_full();
        let (mut lo, mut hi) = ([0u8; 64], [0u8; 64]);
        lo[..32].copy_from_slice(address);
        hi[..32].copy_from_slice(address);
        // The first key after the address's.
        let mut hi = Some(hi);
        if let Some(h) = &mut hi {
            let mut i = 31;
            loop {
                h[i] = h[i].wrapping_add(1);
                if h[i] != 0 {
                    break;
                }
                if i == 0 {
                    hi = None;
                    break;
                }
                i -= 1;
            }
        }
        // Base files first, then segments oldest first: on a version held
        // twice the later input wins, and both hold the same value.
        let mut inputs = Vec::new();
        let first = v.base_index(1, &lo).unwrap_or(0);
        for b in v.base[1].iter().skip(first) {
            if hi.as_ref().is_some_and(|hi| b.first_key >= *hi) {
                break;
            }
            inputs.push(Cursor::new(b.file.clone(), Some(lo), hi, 0, h));
        }
        for s in &v.segs[1] {
            // (Segment filters are over whole keys: they cannot rule an
            // address out.)
            if s.lo <= h {
                inputs.push(Cursor::new(s.file.clone(), Some(lo), hi, 0, h));
            }
        }
        let mut slots: Vec<([u8; 32], [u8; 32])> = Vec::new();
        let mut it = MergeIter::new(inputs)?;
        // Versions come by slot then block: the last of a slot is its value.
        while let Some(r) = it.next()? {
            let slot: [u8; 32] = r.key[32..].try_into().unwrap();
            let Val::Storage(value) = r.val else {
                return Err(corrupt("account value in a storage file"));
            };
            match slots.last_mut() {
                Some(last) if last.0 == slot => last.1 = value,
                _ => slots.push((slot, value)),
            }
        }
        // The memtables hold the newest versions.
        let mut newer: HashMap<[u8; 32], [u8; 32]> = HashMap::new();
        for mem in v.frozen.iter().chain(std::iter::once(&v.active)) {
            for slot in mem.slots_of(address) {
                let mut key = lo;
                key[32..].copy_from_slice(&slot);
                if let Some((_, Val::Storage(value))) = mem.get(true, &key[..], h).found {
                    newer.insert(slot, value);
                }
            }
        }
        if !newer.is_empty() {
            for (slot, value) in slots.iter_mut() {
                if let Some(v) = newer.remove(slot) {
                    *value = v;
                }
            }
            slots.extend(newer);
            slots.sort_unstable();
        }
        slots.retain(|(_, value)| *value != [0u8; 32]);
        Ok(slots)
    }

    /// Write the memtable to segments now.
    pub(crate) fn flush(&self) -> Result<(), Error> {
        let inner = &*self.inner;
        let _w = inner.write.lock().unwrap();
        if inner.opts.background {
            // Hand it to the flusher and wait for it.
            let mut g = inner.flush_lock.lock().unwrap();
            loop {
                inner.check_failed()?;
                inner.freeze();
                inner.flush_cv.notify_all();
                let v = inner.view.load();
                if v.frozen.is_none() && v.active.is_empty() {
                    return Ok(());
                }
                drop(v);
                g = inner
                    .flush_cv
                    .wait_timeout(g, Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
        }
        inner.flush_frozen()?;
        if inner.freeze() {
            inner.flush_frozen()?;
        }
        Ok(())
    }

    /// Hold back every change of the file set while the guard lives: the
    /// directory then is what a crash at this moment would leave.
    #[cfg(test)]
    pub(crate) fn hold_publication(&self) -> impl Drop + '_ {
        self.inner.state.lock().unwrap()
    }

    /// Merge segments until no level has `tiers` of them.
    #[cfg(test)]
    pub(crate) fn merge(&self) -> Result<(), Error> {
        while self.inner.merge_step()? {}
        Ok(())
    }

    /// Fold every segment into the base now, at the store's pace.
    pub(crate) fn rewrite_base(&self) -> Result<(), Error> {
        // The first pass may be the rest of one that was cut short, which
        // leaves what was flushed after it began.
        let resumed = self.inner.state.lock().unwrap().pass != [0; 2];
        self.inner.rewrite_pass()?;
        if resumed {
            self.inner.rewrite_pass()?;
        }
        Ok(())
    }

    /// Go back to block `block`: every version, block info and segment
    /// after it goes. A file that reaches above the block is written again
    /// without what comes after it, base files as well as segments, so the
    /// cost is the size of those files: the segments and, of the base, the
    /// files rewritten since that block. Returns the block the store is at
    /// (`block`, or its head if that is lower); the caller fetches the later
    /// ones again. Offline: the store must have no background work and an
    /// empty memtable.
    ///
    /// Files are trimmed one at a time, each replacing the one it comes
    /// from, so that the space needed is that of one file. In between the
    /// store holds some keys up to `block` and others beyond: the manifest
    /// says that a rewind is under way, a store in that state is opened by
    /// nothing but a rewind, and a rewind that was cut short is run again,
    /// to the same block or one below it.
    pub(crate) fn rewind(&self, block: u64) -> Result<u64, Error> {
        let inner = &*self.inner;
        if inner.opts.background {
            return Err(Error::UnSupported(
                "rewinding a store with background work".into(),
            ));
        }
        let _w = inner.write.lock().unwrap();
        let v = inner.view.load_full();
        if !v.active.is_empty() || v.frozen.is_some() {
            return Err(Error::UnSupported(
                "rewinding a store with a memtable".into(),
            ));
        }
        let Some(durable) = v.durable() else {
            return Err(Error::UnSupported("rewinding an empty store".into()));
        };
        let pending = inner.state.lock().unwrap().rewinding;
        if let Some(pending) = pending.filter(|pending| block > *pending) {
            return Err(Error::UnSupported(format!(
                "a rewind to block {pending} was cut short: files hold nothing above it any \
                 more, rewind to that block or below"
            )));
        }
        if pending.is_none() && block >= durable {
            return Ok(durable);
        }
        if v.blocks.count > 0 && block < v.blocks.first {
            return Err(Error::UnSupported(format!(
                "block {block} is below the first block of the archive"
            )));
        }
        drop(v);
        inner.set_rewinding(Some(block))?;
        let mut trimmed = (0usize, 0u64);
        for kind in 0..2 {
            // Segments wholly after the block go; those across it and the
            // base files that reach beyond it are written again.
            loop {
                let v = inner.view.load_full();
                let Some(seg) = v.segs[kind].iter().find(|s| s.hi > block).cloned() else {
                    break;
                };
                drop(v);
                let new = if seg.lo > block {
                    None
                } else {
                    inner.trim_segment(kind, &seg, block)?
                };
                {
                    let mut st = inner.state.lock().unwrap();
                    let mut nv = (*inner.view.load_full()).clone();
                    let at = nv.segs[kind]
                        .iter()
                        .position(|s| s.file.fid == seg.file.fid)
                        .ok_or_else(|| corrupt("segment vanished during a rewind"))?;
                    nv.segs[kind].splice(at..at + 1, new);
                    inner.publish(&mut st, nv, None)?;
                }
                remove_page_file(&inner.dir.join(&seg.name));
                trimmed = (trimmed.0 + 1, trimmed.1 + seg.file.data_len());
            }
            loop {
                let v = inner.view.load_full();
                let Some(old) = v.base[kind].iter().find(|b| b.hi > block).cloned() else {
                    break;
                };
                drop(v);
                let files = inner.trim_base(kind, &old, block)?;
                {
                    let mut st = inner.state.lock().unwrap();
                    let mut nv = (*inner.view.load_full()).clone();
                    let at = nv.base[kind]
                        .iter()
                        .position(|b| b.file.fid == old.file.fid)
                        .ok_or_else(|| corrupt("base file vanished during a rewind"))?;
                    nv.base[kind].splice(at..at + 1, files);
                    inner.publish(&mut st, nv, None)?;
                }
                remove_page_file(&inner.dir.join(&old.name));
                trimmed = (trimmed.0 + 1, trimmed.1 + old.file.data_len());
            }
        }
        let v = inner.view.load_full();
        let mut nv = (*v).clone();
        let cut = if v.blocks.last().is_some_and(|last| last > block) {
            Some(inner.blocks.cut(block)?)
        } else {
            None
        };
        if let Some(cut) = &cut {
            nv.blocks = cut.meta;
        }
        // Code is found by its hash: what later blocks brought stays.
        let side =
            cut.map(|cut| -> Result<_, Error> { Ok((cut, inner.codes.append([], side::LEVEL)?)) });
        {
            let mut st = inner.state.lock().unwrap();
            // A pass that was folding up to a block now gone starts anew.
            for target in st.pass.iter_mut() {
                if *target > block {
                    *target = 0;
                }
            }
            st.rewinding = None;
            inner.publish(&mut st, nv, side.transpose()?)?;
        }
        let land = inner.view.load().durable().unwrap_or(block);
        inner.next_block.store(land + 1, Ordering::Release);
        info!(
            target: "archive_v4",
            "rewound to block {land} (asked for {block}): {} files of {:.2} GiB written again or dropped",
            trimmed.0,
            trimmed.1 as f64 / (1u64 << 30) as f64
        );
        Ok(land)
    }

    /// Sizes for logs and tools: (base files, base bytes, segments, delta
    /// bytes, index bytes, filter bytes).
    pub(crate) fn sizes(&self) -> (usize, u64, usize, u64, u64, u64) {
        let v = self.inner.view.load();
        (
            v.base.iter().flatten().count(),
            v.base.iter().flatten().map(|b| b.file.data_len()).sum(),
            v.segs.iter().flatten().count(),
            v.segs.iter().flatten().map(|s| s.file.data_len()).sum(),
            index_bytes(&v),
            v.segs.iter().flatten().map(|s| s.bloom.mem_bytes()).sum(),
        )
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        {
            let _g = self.inner.flush_lock.lock().unwrap();
            self.inner.flush_cv.notify_all();
        }
        {
            let _g = self.inner.merge_lock.lock().unwrap();
            self.inner.merge_cv.notify_all();
        }
        {
            let mut woken = self.inner.rewrite_lock.lock().unwrap();
            *woken = true;
            self.inner.rewrite_cv.notify_all();
        }
        for t in self.threads.lock().unwrap().drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// (storage, key, block) -> value.
    pub(crate) type Model = BTreeMap<(bool, [u8; 64], u64), Val>;

    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        pub(crate) fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    pub(crate) fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("leafage-v4-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Small thresholds, so that a few thousand versions exercise every
    /// layer and several base files.
    pub(crate) fn small_options() -> Options {
        Options {
            budget_bytes: 256 << 20,
            flush_bytes: 1 << 30,
            flush_blocks: 16,
            tiers: 3,
            base_file_bytes: 48 << 10,
            base_level: 3,
            rewrite_bytes_per_sec: 0.0,
            delta_ratio: 0.3,
            delta_min_bytes: 0,
            delta_filter_bytes: u64::MAX,
            block_cache_bytes: 1 << 20,
            rewrite_check: Duration::from_millis(20),
            background: false,
        }
    }

    /// The hash of address `a`: spread over the key space, a few sharing
    /// their first bytes.
    pub(crate) fn address(a: u64) -> [u8; 32] {
        let mut r = Rng(a.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let mut k = [0u8; 32];
        for b in k.iter_mut() {
            *b = r.next() as u8;
        }
        if a % 5 == 0 {
            k[..10].fill(0x77);
        }
        k
    }

    pub(crate) fn slot_key(a: u64, s: u64) -> [u8; 64] {
        let mut r = Rng((a << 20 ^ s).wrapping_mul(0xd6e8_feb8_6659_fd93) | 1);
        let mut k = [0u8; 64];
        k[..32].copy_from_slice(&address(a));
        for b in k[32..].iter_mut() {
            *b = r.next() as u8;
        }
        k
    }

    fn word(r: &mut Rng) -> [u8; 32] {
        let mut w = [0u8; 32];
        match r.below(4) {
            0 => {}
            1 => w[31] = r.next() as u8,
            2 => w[20..].iter_mut().for_each(|b| *b = r.next() as u8),
            _ => w.iter_mut().for_each(|b| *b = r.next() as u8),
        }
        w
    }

    /// The versions of one block: accounts and slots, one per key. Address 0
    /// is a large contract; a few keys change in most blocks.
    pub(crate) fn block_diff(
        r: &mut Rng,
        block: u64,
    ) -> (Vec<([u8; 32], Val)>, Vec<([u8; 64], [u8; 32])>) {
        let mut accounts: BTreeMap<[u8; 32], Val> = BTreeMap::new();
        let mut slots: BTreeMap<[u8; 64], [u8; 32]> = BTreeMap::new();
        for _ in 0..r.below(6) {
            let a = if r.below(3) == 0 { 1 } else { r.below(120) };
            let val = if r.below(15) == 0 {
                Val::Deleted
            } else {
                Val::Account {
                    nonce: block / (1 + a % 3),
                    balance: word(r),
                    code_hash: if a % 4 == 0 {
                        codec::KECCAK_EMPTY
                    } else {
                        address(a + 1000)
                    },
                }
            };
            accounts.insert(address(a), val);
        }
        for _ in 0..r.below(40) {
            let key = match r.below(10) {
                0..=4 => slot_key(0, r.below(3000)),
                5 => slot_key(7, 1),
                _ => slot_key(1 + r.below(60), r.below(12)),
            };
            slots.insert(key, word(r));
        }
        (accounts.into_iter().collect(), slots.into_iter().collect())
    }

    pub(crate) fn apply_block(store: &Store, model: &mut Model, r: &mut Rng, block: u64) {
        let (accounts, slots) = block_diff(r, block);
        store.apply(block, &accounts, &slots).unwrap();
        store.flush_if_due().unwrap();
        for (a, v) in accounts {
            let mut k = [0u8; 64];
            k[..32].copy_from_slice(&a);
            model.insert((false, k, block), v);
        }
        for (k, v) in slots {
            model.insert((true, k, block), Val::Storage(v));
        }
    }

    /// Read every key at heights around each of its versions and compare
    /// with the model; returns (reads, reads whose `next` was exact).
    pub(crate) fn check(store: &Store, model: &Model, head: u64) -> (u64, u64) {
        let mut keys: Vec<(bool, [u8; 64])> = model.keys().map(|k| (k.0, k.1)).collect();
        keys.dedup();
        let (mut reads, mut exact) = (0u64, 0u64);
        let mut probe = |storage: bool, key: &[u8; 64], h: u64| {
            let k = key_len(storage);
            let want = model
                .range((storage, *key, 0)..=(storage, *key, h))
                .next_back()
                .map(|(kb, v)| (kb.2, *v));
            let true_next = model
                .range((storage, *key, h + 1)..=(storage, *key, u64::MAX))
                .next()
                .map_or(u64::MAX, |(kb, _)| kb.2);
            let got = store.get(storage, &key[..k], h).unwrap();
            assert_eq!(
                got.found,
                want,
                "storage {storage} key {:x?} at {h}",
                &key[..4]
            );
            assert!(
                got.next > h && got.next <= true_next,
                "next {} for a read at {h}, real next {true_next}",
                got.next
            );
            reads += 1;
            exact += (got.next == true_next) as u64;
        };
        for (storage, key) in &keys {
            let blocks: Vec<u64> = model
                .range((*storage, *key, 0)..=(*storage, *key, u64::MAX))
                .map(|(kb, _)| kb.2)
                .collect();
            let mut heights = vec![0, head, head + 3];
            // Every version of a key with few, the ends of a long history.
            let pick: Vec<u64> = if blocks.len() <= 8 {
                blocks.clone()
            } else {
                blocks
                    .iter()
                    .copied()
                    .step_by(blocks.len() / 6)
                    .chain(blocks.last().copied())
                    .collect()
            };
            for b in pick {
                heights.extend([b.saturating_sub(1), b, b + 1]);
            }
            heights.sort_unstable();
            heights.dedup();
            for h in heights {
                probe(*storage, key, h);
            }
        }
        // Keys never written: next to written ones, and anywhere.
        let mut r = Rng(head | 1);
        for i in 0..400u64 {
            let mut key = if i % 2 == 0 {
                slot_key(r.below(70), 5000 + i)
            } else {
                keys[r.below(keys.len() as u64) as usize].1
            };
            key[63] ^= 0x5a;
            key[40] = key[40].wrapping_add(i as u8);
            if !model.contains_key(&(true, key, 0))
                && model
                    .range((true, key, 0)..=(true, key, u64::MAX))
                    .next()
                    .is_none()
            {
                probe(true, &key, r.below(head + 2));
            }
        }
        (reads, exact)
    }

    fn check_slots(store: &Store, model: &Model, a: u64, h: u64) {
        let addr = address(a);
        let mut want: BTreeMap<[u8; 32], [u8; 32]> = BTreeMap::new();
        let (mut lo, mut hi) = ([0u8; 64], [0xffu8; 64]);
        lo[..32].copy_from_slice(&addr);
        hi[..32].copy_from_slice(&addr);
        for ((_, key, block), val) in model.range((true, lo, 0)..=(true, hi, u64::MAX)) {
            if *block <= h {
                let Val::Storage(v) = val else { unreachable!() };
                want.insert(key[32..].try_into().unwrap(), *v);
            }
        }
        want.retain(|_, v| *v != [0u8; 32]);
        let got = store.storage_slots(&addr, h).unwrap();
        let want: Vec<_> = want.into_iter().collect();
        if got != want {
            let g: BTreeMap<_, _> = got.iter().cloned().collect();
            let w: BTreeMap<_, _> = want.iter().cloned().collect();
            for (k, v) in &w {
                if g.get(k) != Some(v) {
                    eprintln!(
                        "slot {:x?}: want {:x?} got {:x?}",
                        &k[..4],
                        &v[28..],
                        g.get(k).map(|x| x[28..].to_vec())
                    );
                }
            }
            for k in g.keys().filter(|k| !w.contains_key(*k)) {
                eprintln!("slot {:x?}: unexpected", &k[..4]);
            }
            panic!("slots of {a} at {h}: {} vs {}", got.len(), want.len());
        }
    }

    /// Apply the model's blocks above the store's durable head again: what
    /// the node does after a restart, with the blocks it fetches again.
    fn replay(store: &Store, model: &Model, head: u64) {
        let from = store.durable().map_or(0, |d| d + 1);
        for block in from..=head {
            let mut accounts = Vec::new();
            let mut slots = Vec::new();
            for ((storage, key, b), val) in model.iter() {
                if *b != block {
                    continue;
                }
                match (storage, val) {
                    (true, Val::Storage(v)) => slots.push((*key, *v)),
                    (false, v) => accounts.push((key[..32].try_into().unwrap(), *v)),
                    _ => unreachable!(),
                }
            }
            store.apply(block, &accounts, &slots).unwrap();
        }
    }

    /// A base rewrite pass that was cut short goes on with the files it had
    /// not reached, and leaves the ones it had rewritten as they are, though
    /// segments flushed since are above them.
    #[test]
    fn interrupted_rewrite_pass_resumes() {
        let dir = test_dir("resume-pass");
        Store::create(&dir).unwrap();
        let store = Store::open(&dir, small_options()).unwrap();
        let mut model = Model::new();
        let mut r = Rng(0x0dd_ba11_5eed);
        let mut head = 0;
        let mut blocks = |store: &Store, model: &mut Model, n: u64| {
            for _ in 0..n {
                head += 1;
                apply_block(store, model, &mut r, head);
            }
            store.flush().unwrap();
            head
        };
        blocks(&store, &mut model, 300);
        store.rewrite_base().unwrap();
        blocks(&store, &mut model, 90);

        // The pass starts on the storage files and gets through the first.
        let inner = &store.inner;
        let v = inner.view.load_full();
        assert!(v.base[1].len() >= 3, "{} base files", v.base[1].len());
        let target = v.segs[1].iter().map(|s| s.hi).max().unwrap();
        inner.set_pass(1, target).unwrap();
        let first = v.base[1][0].clone();
        let n = inner
            .rewrite_range(1, Some(&first), &mut Throttle::new(inner))
            .unwrap();
        assert!(n >= 1);
        let names = |v: &View, range: std::ops::Range<usize>| -> Vec<String> {
            v.base[1][range].iter().map(|b| b.name.clone()).collect()
        };
        let v = inner.view.load_full();
        let (done, behind) = (names(&v, 0..n), names(&v, n..v.base[1].len()));
        assert!(!behind.is_empty());
        assert!(v.segs[1].iter().any(|s| s.hi <= target), "segments kept");

        // More blocks are flushed, and the node stops.
        let head = blocks(&store, &mut model, 40);
        drop(store);
        let store = Store::open(&dir, small_options()).unwrap();
        let inner = &store.inner;
        assert_eq!(inner.state.lock().unwrap().pass, [0, target]);
        inner.rewrite_pass().unwrap();
        let v = inner.view.load_full();
        let now = names(&v, 0..v.base[1].len());
        for name in &done {
            assert!(now.contains(name), "{name} was rewritten again");
        }
        for name in &behind {
            assert!(!now.contains(name), "{name} was left behind");
        }
        // What the pass was after is folded; what came later is delta.
        assert!(v.base[1].iter().all(|b| b.hi >= target));
        assert!(v.segs[1].iter().all(|s| s.hi > target));
        assert!(!v.segs[1].is_empty());
        assert_eq!(inner.state.lock().unwrap().pass, [0, 0]);
        check(&store, &model, head);
        check_slots(&store, &model, 0, head);

        // The next pass takes everything.
        store.rewrite_base().unwrap();
        assert_eq!(store.sizes().2, 0, "every segment folded");
        check(&store, &model, head);
        drop(store);
        let store = Store::open(&dir, small_options()).unwrap();
        check(&store, &model, head);
    }

    #[test]
    fn layers_agree_with_a_model_through_flush_merge_rewrite_and_reopen() {
        let dir = test_dir("model");
        Store::create(&dir).unwrap();
        let mut store = Store::open(&dir, small_options()).unwrap();
        let mut model = Model::new();
        let mut r = Rng(0x1234_5678_9abc_def1);
        let mut head = 0;
        for round in 0..6 {
            for _ in 0..90 {
                head += 1;
                apply_block(&store, &mut model, &mut r, head);
                if head % 7 == 0 {
                    // An empty block only moves the head.
                    head += 1;
                    store.apply(head, &[], &[]).unwrap();
                    store.flush_if_due().unwrap();
                }
            }
            assert_eq!(store.head(), Some(head));
            check(&store, &model, head);
            check_slots(&store, &model, 0, head);
            check_slots(&store, &model, 0, head / 2);
            check_slots(&store, &model, 7, head);
            match round {
                0 => store.merge().unwrap(),
                1 => {
                    // First base: everything flushed so far.
                    store.rewrite_base().unwrap();
                    let (files, bytes, ..) = store.sizes();
                    assert!(files >= 3, "{files} base files, {bytes} bytes");
                }
                2 => {
                    store.flush().unwrap();
                    store.merge().unwrap();
                }
                3 => {
                    store.rewrite_base().unwrap();
                    // Only what was flushed after the rewrite began is delta.
                    assert!(store.inner.view.load().delta(1).count() <= 1);
                }
                4 => {
                    // Restart: the memtable is gone, its blocks come again.
                    drop(store);
                    store = Store::open(&dir, small_options()).unwrap();
                    assert!(store.durable().unwrap() < head);
                    replay(&store, &model, head);
                }
                _ => {
                    store.flush().unwrap();
                    store.rewrite_base().unwrap();
                    assert_eq!(store.sizes().2, 0, "every segment folded");
                }
            }
            let (reads, exact) = check(&store, &model, head);
            // Once everything is in the base, the span of an answer is exact
            // (a segment above the read height only gives its first block).
            if round == 5 {
                assert!(exact * 100 >= reads * 99, "{exact} of {reads} spans exact");
            }
            check_slots(&store, &model, 0, head);
            check_slots(&store, &model, 3, head - 40);
        }
        // Writing at or below the head is refused; the head block again is not.
        let key = slot_key(2, 2);
        assert!(store.apply(head - 5, &[], &[(key, [1; 32])]).is_err());
        store.apply(head + 1, &[], &[(key, [1; 32])]).unwrap();
        store.apply(head + 1, &[], &[(key, [2; 32])]).unwrap();
        assert_eq!(
            store.get(true, &key, head + 1).unwrap().found,
            Some((head + 1, Val::Storage([2; 32])))
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rewind_drops_later_versions_from_every_layer() {
        let dir = test_dir("rewind");
        Store::create(&dir).unwrap();
        let store = Store::open(&dir, small_options()).unwrap();
        let mut model = Model::new();
        let mut r = Rng(77);
        for block in 1..=150 {
            apply_block(&store, &mut model, &mut r, block);
        }
        store.rewrite_base().unwrap();
        for block in 151..=260 {
            apply_block(&store, &mut model, &mut r, block);
        }
        store.flush().unwrap();
        drop(store);
        // Into the delta, to the very block, in the middle of a segment or
        // at its end; a block at or above the head changes nothing.
        let mut head = 260;
        for target in [233, 201, 201, 300] {
            let store = Store::open(&dir, small_options()).unwrap();
            let landed = store.rewind(target).unwrap();
            assert_eq!(landed, target.min(head));
            assert_eq!(store.durable(), Some(landed));
            assert_eq!(store.head(), Some(landed));
            head = landed;
            model.retain(|k, _| k.2 <= head);
            check(&store, &model, head);
            check_slots(&store, &model, 0, head);
            drop(store);
        }
        // Into the base, which holds the blocks flushed before it was
        // rewritten (144 of the 150, in memtables of 16): its files are
        // written again without what came after the block.
        {
            let store = Store::open(&dir, small_options()).unwrap();
            assert_eq!(store.rewind(120).unwrap(), 120);
            head = 120;
            model.retain(|k, _| k.2 <= head);
            assert_eq!(store.sizes().2, 0, "no segment is left");
            let v = store.inner.view.load_full();
            assert!(v.base.iter().flatten().all(|b| b.hi == 120));
            check(&store, &model, head);
            check_slots(&store, &model, 0, head);
            drop(v);
            drop(store);
            let store = Store::open(&dir, small_options()).unwrap();
            assert_eq!(store.head(), Some(120));
            check(&store, &model, head);
        }
        // A rewind that was cut short after its first base file: the store
        // is opened by nothing but a rewind, to that block or below, which
        // then goes on.
        {
            let store = Store::open(&dir, small_options()).unwrap();
            let inner = &store.inner;
            inner.set_rewinding(Some(100)).unwrap();
            let v = inner.view.load_full();
            let old = v.base[1][0].clone();
            let files = inner.trim_base(1, &old, 100).unwrap();
            {
                let mut st = inner.state.lock().unwrap();
                let mut nv = (*v).clone();
                nv.base[1].splice(0..1, files);
                inner.publish(&mut st, nv, None).unwrap();
            }
            remove_page_file(&dir.join(&old.name));
            drop(v);
            drop(store);
            let mut serving = small_options();
            serving.background = true;
            assert!(Store::open(&dir, serving).is_err());
            let store = Store::open(&dir, small_options()).unwrap();
            assert!(store.rewind(110).is_err());
            assert_eq!(store.rewind(100).unwrap(), 100);
            head = 100;
            model.retain(|k, _| k.2 <= head);
            check(&store, &model, head);
            check_slots(&store, &model, 0, head);
            drop(store);
            let mut serving = small_options();
            serving.background = true;
            drop(Store::open(&dir, serving).unwrap());
        }
        // A different chain grows from the rewind point.
        let store = Store::open(&dir, small_options()).unwrap();
        let mut r = Rng(4242);
        for block in store.durable().map_or(0, |d| d + 1)..=220 {
            apply_block(&store, &mut model, &mut r, block);
        }
        store.flush().unwrap();
        store.rewrite_base().unwrap();
        check(&store, &model, 220);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Block infos and code go through the memtable into their files with
    /// the versions of the same flush, are read from either, and what was
    /// not flushed is gone after a reopen.
    #[test]
    fn blocks_and_code_commit_with_their_segments() {
        let dir = test_dir("side");
        Store::create(&dir).unwrap();
        let rec = |n: u64, fork: u8| -> BlockRec {
            let mut hash = [fork; 32];
            hash[..8].copy_from_slice(&n.to_be_bytes());
            (hash, Arc::from(vec![n as u8; (n % 50) as usize + 1]))
        };
        let code = |i: u64| -> ([u8; 32], Arc<[u8]>) {
            let mut hash = [0xc0u8; 32];
            hash[..8].copy_from_slice(&i.to_be_bytes());
            (hash, Arc::from(vec![i as u8; i as usize % 90 + 1]))
        };
        let write = |store: &Store, n: u64, fork: u8| -> Result<(), Error> {
            let slot = (slot_key(1, n % 5), [n as u8; 32]);
            // Every third block has no versions; one in seven no info.
            let versions: Vec<BlockVersions> = if n % 3 == 0 {
                vec![(n, &[], &[])]
            } else {
                vec![(n, &[], std::slice::from_ref(&slot))]
            };
            let blocks: Vec<(u64, BlockRec)> = if n % 7 == 6 {
                Vec::new()
            } else {
                vec![(n, rec(n, fork))]
            };
            store.commit(&versions, &blocks, &[code(n / 4)])
        };
        let check = |store: &Store, last: u64, fork_from: u64| {
            assert_eq!(store.head(), Some(last));
            let mut hashes = Vec::new();
            store
                .scan_block_prefixes(0, u64::MAX, &mut |n, prefix| {
                    hashes.push((n, prefix));
                    Ok(())
                })
                .unwrap();
            let mut want = Vec::new();
            for n in 10..=last {
                let fork = u8::from(n >= fork_from);
                let held = (n % 7 != 6).then(|| rec(n, fork));
                assert_eq!(store.block(n).unwrap(), held, "block {n}");
                assert_eq!(store.block_hash(n).unwrap(), held.as_ref().map(|r| r.0));
                want.extend(held.map(|r| (n, u64::from_be_bytes(r.0[..8].try_into().unwrap()))));
                assert_eq!(
                    store.code(&code(n / 4).0).unwrap(),
                    Some(code(n / 4).1.to_vec())
                );
            }
            assert_eq!(hashes, want);
            assert_eq!(store.block(9).unwrap(), None);
            assert_eq!(store.block(last + 1).unwrap(), None);
            assert_eq!(store.code(&[1u8; 32]).unwrap(), None);
        };
        let store = Store::open(&dir, small_options()).unwrap();
        // Memtables of 16 blocks: several flushed, the last blocks not.
        for n in 10..=75 {
            write(&store, n, 0).unwrap();
            store.flush_if_due().unwrap();
        }
        check(&store, 75, u64::MAX);
        let durable = store.durable().unwrap();
        assert!(durable < 75 && durable >= 57, "flushed through {durable}");
        // The head block again: as the block it is, in the memtable or in
        // the files; another block in its place is refused.
        store.commit(&[], &[(75, rec(75, 0))], &[]).unwrap();
        assert!(store.commit(&[], &[(75, rec(75, 1))], &[]).is_err());
        assert_eq!(store.block(75).unwrap(), Some(rec(75, 0)));
        assert!(store.commit(&[], &[(74, rec(74, 0))], &[]).is_err());
        store
            .commit(&[], &[(durable, rec(durable, 0))], &[])
            .unwrap();
        assert!(store
            .commit(&[], &[(durable, rec(durable, 1))], &[])
            .is_err());
        drop(store);

        let store = Store::open(&dir, small_options()).unwrap();
        check(&store, durable, u64::MAX);
        // Another chain from there, flushed.
        for n in durable + 1..=120 {
            write(&store, n, 1).unwrap();
            store.flush_if_due().unwrap();
        }
        store.flush().unwrap();
        check(&store, 120, durable + 1);
        drop(store);
        let store = Store::open(&dir, small_options()).unwrap();
        check(&store, 120, durable + 1);
        // A rewind cuts the block files where the segments end.
        let landed = store.rewind(100).unwrap();
        assert!(landed <= 100 && landed > durable);
        check(&store, landed, durable + 1);
        drop(store);
        let store = Store::open(&dir, small_options()).unwrap();
        check(&store, landed, durable + 1);
        for n in landed + 1..=130 {
            write(&store, n, 1).unwrap();
        }
        store.flush().unwrap();
        check(&store, 130, durable + 1);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A reader does not wait for the writer: a block it can see (the head
    /// is at it, or its info is there) has its versions and its code.
    #[test]
    fn a_block_is_seen_with_all_it_wrote() {
        let dir = test_dir("commit-order");
        Store::create(&dir).unwrap();
        let store = Arc::new(Store::open(&dir, small_options()).unwrap());
        let account = |n: u64| -> ([u8; 32], Val) {
            let mut hash = [0xc0u8; 32];
            hash[..8].copy_from_slice(&n.to_be_bytes());
            let val = Val::Account {
                nonce: n,
                balance: [0; 32],
                code_hash: hash,
            };
            (address(n), val)
        };
        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let (store, done) = (store.clone(), done.clone());
            std::thread::spawn(move || {
                let mut seen = 0u64;
                while !done.load(Ordering::Relaxed) {
                    let Some(head) = store.head() else { continue };
                    let via_info = (head + 1..head + 4)
                        .rev()
                        .find(|n| store.block(*n).unwrap().is_some());
                    for n in [Some(head), via_info].into_iter().flatten() {
                        let (address, val) = account(n);
                        let mut key = [0u8; 64];
                        key[..32].copy_from_slice(&address);
                        let read = store.get(false, &key[..32], n).unwrap();
                        assert_eq!(read.found, Some((n, val)), "block {n}");
                        let Val::Account { code_hash, .. } = val else {
                            unreachable!()
                        };
                        assert!(store.code(&code_hash).unwrap().is_some(), "code of {n}");
                        seen += 1;
                    }
                }
                seen
            })
        };
        // Batches of three blocks: each deploys an account, with its code,
        // and writes enough slots for a reader to get in between.
        let mut n = 1u64;
        while n <= 600 {
            let blocks: Vec<u64> = (n..n + 3).collect();
            let accounts: Vec<[([u8; 32], Val); 1]> =
                blocks.iter().map(|b| [account(*b)]).collect();
            let slots: Vec<Vec<([u8; 64], [u8; 32])>> = blocks
                .iter()
                .map(|b| {
                    (0..200)
                        .map(|s| (slot_key(1, b * 1000 + s), [*b as u8; 32]))
                        .collect()
                })
                .collect();
            let versions: Vec<BlockVersions> = blocks
                .iter()
                .enumerate()
                .map(|(i, b)| (*b, &accounts[i][..], &slots[i][..]))
                .collect();
            let infos: Vec<(u64, BlockRec)> = blocks
                .iter()
                .map(|b| {
                    let mut hash = [7u8; 32];
                    hash[..8].copy_from_slice(&b.to_be_bytes());
                    (*b, (hash, Arc::from(vec![1u8; 40])))
                })
                .collect();
            let codes: Vec<([u8; 32], Arc<[u8]>)> = blocks
                .iter()
                .map(|b| {
                    let Val::Account { code_hash, .. } = account(*b).1 else {
                        unreachable!()
                    };
                    (code_hash, Arc::from(vec![*b as u8; 100]))
                })
                .collect();
            store.commit(&versions, &infos, &codes).unwrap();
            store.flush_if_due().unwrap();
            n += 3;
        }
        done.store(true, Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// While a base rewrite pass is under way, segments it will drop are
    /// not merged with later ones: the merged one would outlive the pass.
    #[test]
    fn segments_do_not_merge_across_the_block_a_pass_folds_up_to() {
        let dir = test_dir("merge-pass");
        Store::create(&dir).unwrap();
        let store = Store::open(&dir, small_options()).unwrap();
        let mut model = Model::new();
        let mut r = Rng(0x5e9_0001);
        let mut head = 0;
        let mut segment = |store: &Store, model: &mut Model| {
            for _ in 0..5 {
                head += 1;
                apply_block(store, model, &mut r, head);
            }
            store.flush().unwrap();
            head
        };
        segment(&store, &mut model);
        let target = segment(&store, &mut model);
        store.inner.set_pass(1, target).unwrap();
        segment(&store, &mut model);
        segment(&store, &mut model);
        // Four storage segments, tiers of three: every three neighbours
        // have segments on both sides of the target.
        store.merge().unwrap();
        assert_eq!(store.inner.view.load().segs[1].len(), 4);
        let head = segment(&store, &mut model);
        store.merge().unwrap();
        let v = store.inner.view.load_full();
        assert_eq!(v.segs[1].len(), 3, "the three after the target merged");
        for s in &v.segs[1] {
            assert!(s.hi <= target || s.lo > target, "{}..={}", s.lo, s.hi);
        }
        check(&store, &model, head);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Time a throttle was not used gives no credit: what comes after it
    /// is paced like anything else.
    #[test]
    fn a_throttle_does_not_make_up_for_idle_time() {
        let dir = test_dir("throttle");
        Store::create(&dir).unwrap();
        let mut opts = small_options();
        opts.rewrite_bytes_per_sec = 1e6;
        let store = Store::open(&dir, opts).unwrap();
        let mut throttle = Throttle::new(&store.inner);
        std::thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        throttle.spend(600_000).unwrap();
        // 0.6 s at the rate, less the burst.
        assert!(started.elapsed() >= Duration::from_millis(350));
        // Merges and rewrites pay from one budget: what one spent, the
        // other waits for.
        let mut other = Throttle::new(&store.inner);
        let started = Instant::now();
        other.spend(300_000).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(250));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn background_work_keeps_reads_right_under_writes() {
        let dir = test_dir("background");
        Store::create(&dir).unwrap();
        let mut opts = small_options();
        opts.background = true;
        opts.flush_blocks = 8;
        let store = Arc::new(Store::open(&dir, opts).unwrap());
        // The whole history up front, so that readers can check any height
        // already applied.
        let mut model = Model::new();
        let mut diffs = Vec::new();
        let mut r = Rng(99);
        for block in 1..=400u64 {
            let (accounts, slots) = block_diff(&mut r, block);
            for (a, v) in &accounts {
                let mut k = [0u8; 64];
                k[..32].copy_from_slice(a);
                model.insert((false, k, block), *v);
            }
            for (k, v) in &slots {
                model.insert((true, *k, block), Val::Storage(*v));
            }
            diffs.push((accounts, slots));
        }
        let model = Arc::new(model);
        let applied = Arc::new(AtomicU64::new(0));
        let keys: Arc<Vec<(bool, [u8; 64])>> = Arc::new({
            let mut k: Vec<_> = model.keys().map(|k| (k.0, k.1)).collect();
            k.dedup();
            k
        });
        let readers: Vec<_> = (0..4u64)
            .map(|t| {
                let (store, model, applied, keys) =
                    (store.clone(), model.clone(), applied.clone(), keys.clone());
                std::thread::spawn(move || {
                    let mut r = Rng(1000 + t);
                    let mut n = 0u64;
                    while applied.load(Ordering::Acquire) < 400 || n < 20_000 {
                        let head = applied.load(Ordering::Acquire);
                        if head == 0 {
                            std::thread::yield_now();
                            continue;
                        }
                        let (storage, key) = keys[r.below(keys.len() as u64) as usize];
                        let h = 1 + r.below(head);
                        let want = model
                            .range((storage, key, 0)..=(storage, key, h))
                            .next_back()
                            .map(|(kb, v)| (kb.2, *v));
                        // The real next version, among those applied when
                        // the read began.
                        let next = if h >= head {
                            u64::MAX
                        } else {
                            model
                                .range((storage, key, h + 1)..=(storage, key, head))
                                .next()
                                .map_or(u64::MAX, |(kb, _)| kb.2)
                        };
                        let got = store.get(storage, &key[..key_len(storage)], h).unwrap();
                        assert_eq!(got.found, want);
                        assert!(
                            got.next > h && got.next <= next,
                            "read at {h} with head {head}: next {} but the key's next version is at {next}",
                            got.next
                        );
                        n += 1;
                    }
                    n
                })
            })
            .collect();
        for (i, (accounts, slots)) in diffs.iter().enumerate() {
            let block = i as u64 + 1;
            store.apply(block, accounts, slots).unwrap();
            store.flush_if_due().unwrap();
            applied.store(block, Ordering::Release);
            if block % 100 == 0 {
                // The rewriter runs on its own timer; make it work now.
                store.rewrite_base().unwrap();
            }
        }
        for t in readers {
            assert!(t.join().unwrap() >= 20_000);
        }
        store.flush().unwrap();
        check(&store, &model, 400);
        assert!(store.durable() == Some(400) || store.durable() == Some(399));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_removes_leftovers_and_reports_damage() {
        let dir = test_dir("damage");
        Store::create(&dir).unwrap();
        let store = Store::open(&dir, small_options()).unwrap();
        let mut model = Model::new();
        let mut r = Rng(5);
        for block in 1..=64 {
            apply_block(&store, &mut model, &mut r, block);
        }
        store.flush().unwrap();
        drop(store);
        // What an interrupted flush leaves behind, and an address filter of
        // an earlier build with its line in the manifest.
        for name in [
            "seg-00009999-s.v4",
            "seg-00009999-s.cidx",
            "MANIFEST.tmp",
            "dict-000777.dict",
            "flt-00000001-0123456789abcdef01234567.bloom",
        ] {
            std::fs::write(dir.join(name), b"partial").unwrap();
        }
        let manifest = std::fs::read_to_string(dir.join(MANIFEST)).unwrap();
        let line = format!(
            "filter 1 {} 5 flt-00000001-0123456789abcdef01234567\n",
            "01".repeat(32)
        );
        std::fs::write(dir.join(MANIFEST), manifest + &line).unwrap();
        std::fs::write(dir.join("unrelated"), b"kept").unwrap();
        let store = Store::open(&dir, small_options()).unwrap();
        assert!(!dir.join("seg-00009999-s.v4").exists());
        assert!(!dir.join("dict-000777.dict").exists());
        assert!(!dir
            .join("flt-00000001-0123456789abcdef01234567.bloom")
            .exists());
        assert!(dir.join("unrelated").exists());
        check(&store, &model, 64);
        let seg = store.inner.view.load().segs[1][0].name.clone();
        drop(store);

        // A flipped bit in a page is an error, not a wrong answer.
        let path = file::data_path(&dir.join(&seg));
        let mut bytes = std::fs::read(&path).unwrap();
        let at = bytes.len() / 2;
        bytes[at] ^= 0x10;
        std::fs::write(&path, &bytes).unwrap();
        let store = Store::open(&dir, small_options()).unwrap();
        let mut errors = 0;
        for (storage, key, _) in model.keys().filter(|k| k.0) {
            match store.get(*storage, key, 64) {
                Ok(read) => {
                    let want = model
                        .range((true, *key, 0)..=(true, *key, 64))
                        .next_back()
                        .map(|(kb, v)| (kb.2, *v));
                    assert_eq!(read.found, want);
                }
                Err(_) => errors += 1,
            }
        }
        assert!(errors > 0, "the damaged page was read");
        drop(store);

        // A truncated file fails the open.
        std::fs::write(&path, &bytes[..bytes.len() - 64]).unwrap();
        assert!(Store::open(&dir, small_options()).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod weight_tests {
    use super::*;

    #[test]
    fn a_page_weighs_its_allocation() {
        let weigh = |len: usize| PageWeighter.weight(&0, &Arc::from(vec![0u8; len])) - 96;
        // jemalloc's classes around a page: 10K, 12K, 14K, 16K, 20K.
        assert_eq!(weigh(12_000), 12_288);
        assert_eq!(weigh(12_288 - 16), 12_288);
        assert_eq!(weigh(12_288), 14_336);
        assert_eq!(weigh(16_000), 16_384);
        assert_eq!(weigh(17_000), 20_480);
        assert_eq!(weigh(10), 64);
    }
}

/// Not a test: prints the cost of lookups on a small store whose pages are
/// all cached. `cargo test --release -p leafage-evm-storage --lib
/// lookup_cost -- --ignored --nocapture`.
#[cfg(test)]
mod lookup_cost {
    use super::tests::{slot_key, test_dir, Rng};
    use super::*;

    #[test]
    #[ignore]
    fn lookup_cost() {
        let dir = test_dir("cost");
        Store::create(&dir).unwrap();
        let mut opts = Options::maintenance(4 << 30);
        opts.flush_blocks = 2000;
        opts.base_file_bytes = 64 << 20;
        let store = Store::open(&dir, opts).unwrap();
        let mut r = Rng(0xfeed_beef_1234_5677);
        // 2000 contracts of 500 slots and one of a million, four million versions.
        let key = |r: &mut Rng| {
            if r.below(4) == 0 {
                slot_key(0, r.below(1_000_000))
            } else {
                slot_key(1 + r.below(2000), r.below(500))
            }
        };
        for block in 1..=8000u64 {
            let mut slots: std::collections::BTreeMap<[u8; 64], [u8; 32]> = Default::default();
            for _ in 0..500 {
                let mut v = [0u8; 32];
                v[24..].copy_from_slice(&r.next().to_be_bytes());
                slots.insert(key(&mut r), v);
            }
            let slots: Vec<_> = slots.into_iter().collect();
            store.apply(block, &[], &slots).unwrap();
            store.flush_if_due().unwrap();
        }
        store.flush().unwrap();
        store.rewrite_base().unwrap();
        let (files, bytes, ..) = store.sizes();
        println!(
            "{files} base files, {:.1} MiB",
            bytes as f64 / (1 << 20) as f64
        );
        let probes: Vec<([u8; 64], u64)> = (0..2_000_000)
            .map(|i| {
                let mut k = key(&mut r);
                if i % 5 == 0 {
                    k[63] ^= 1; // absent
                }
                (k, 1 + r.below(8000))
            })
            .collect();
        for pass in 0..3 {
            for threads in [1usize, 8] {
                let t = Instant::now();
                let found: u64 = std::thread::scope(|scope| {
                    let workers: Vec<_> = (0..threads)
                        .map(|w| {
                            let (store, probes) = (&store, &probes);
                            scope.spawn(move || {
                                let mut found = 0u64;
                                for (k, h) in probes.iter().skip(w).step_by(threads) {
                                    found += store.get(true, k, *h).unwrap().found.is_some() as u64;
                                }
                                found
                            })
                        })
                        .collect();
                    workers.into_iter().map(|w| w.join().unwrap()).sum()
                });
                let secs = t.elapsed().as_secs_f64();
                println!(
                    "pass {pass}, {threads} threads: {:.2} us/get of one thread, {:.0} gets/s, {found} found",
                    secs * 1e6 * threads as f64 / probes.len() as f64,
                    probes.len() as f64 / secs
                );
            }
        }
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
