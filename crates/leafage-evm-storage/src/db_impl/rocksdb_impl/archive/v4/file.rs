//! Page files of archive schema v4: `<name>.v4` (pages) with `<name>.cidx`
//! (the index, see [`super::index`]) and, for delta segments, `<name>.bloom`.
//!
//! The data file is a sequence of stored pages, each
//! `clen u32 LE | zstd frame | zero padding` up to a multiple of 64 bytes.
//! Frames are compressed with the file's dictionary and carry zstd's
//! content checksum. Files are written once and never modified.

use super::codec::{self, key_len, rd32, rd64, PageEnc, Val, PAGE_TARGET};
use super::index::{Index, IndexBuilder, UNIT};
use super::{corrupt, io_stats};
use crate::db_impl::error::Error;
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::cell::RefCell;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zstd::zstd_safe;

/// Alignment of direct reads.
const ALIGN: usize = 4096;
/// Largest raw page accepted: far above what the encoder produces.
const MAX_RAW_PAGE: usize = 4 << 20;
/// Sequential read window.
const SEQ_WINDOW: usize = 4 << 20;
/// A writer hands its dirty pages to the device and drops them from the
/// page cache this often, so that a large rewrite does not push the node's
/// own memory out.
const WRITE_SYNC_BYTES: u64 = 32 << 20;

/// A zstd dictionary shared by page files.
pub(crate) struct Dict {
    pub(crate) id: u32,
    pub(crate) raw: Vec<u8>,
    dd: zstd::dict::DecoderDictionary<'static>,
}

impl Dict {
    pub(crate) fn new(id: u32, raw: Vec<u8>) -> Self {
        let dd = zstd::dict::DecoderDictionary::copy(&raw);
        Self { id, raw, dd }
    }
}

struct AlignedBuf {
    ptr: *mut u8,
    cap: usize,
}

impl AlignedBuf {
    fn new(cap: usize) -> Self {
        let cap = cap.max(ALIGN).div_ceil(ALIGN) * ALIGN;
        let ptr = unsafe { alloc_zeroed(Layout::from_size_align(cap, ALIGN).unwrap()) };
        assert!(!ptr.is_null(), "out of memory");
        Self { ptr, cap }
    }

    /// The first `n` bytes, growing the buffer if needed (contents are not
    /// kept).
    fn slice(&mut self, n: usize) -> &mut [u8] {
        if n > self.cap {
            *self = Self::new(n);
        }
        unsafe { std::slice::from_raw_parts_mut(self.ptr, n) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr, Layout::from_size_align(self.cap, ALIGN).unwrap()) }
    }
}

unsafe impl Send for AlignedBuf {}

/// Decompress one stored page (`clen | frame | padding`) into `out`.
fn decode_stored(
    dctx: &mut zstd_safe::DCtx<'static>,
    dict: &Dict,
    stored: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    if stored.len() < 4 {
        return Err(corrupt("stored page too short"));
    }
    let clen = rd32(stored, 0) as usize;
    let frame = stored
        .get(4..4 + clen)
        .ok_or_else(|| corrupt("stored page length out of range"))?;
    let size = match zstd_safe::get_frame_content_size(frame) {
        Ok(Some(n)) if n as usize <= MAX_RAW_PAGE => n as usize,
        _ => return Err(corrupt("stored page has no valid size")),
    };
    out.clear();
    out.reserve(size);
    dctx.decompress_using_ddict(out, frame, dict.dd.as_ddict())
        .map_err(|code| {
            corrupt(format!(
                "page does not decompress: {}",
                zstd_safe::get_error_name(code)
            ))
        })?;
    if out.len() != size {
        return Err(corrupt("page decompressed to the wrong size"));
    }
    Ok(())
}

/// Read `[start, start + buf.len())`, short only at the end of the file.
fn read_at(file: &File, buf: &mut [u8], start: u64) -> Result<usize, Error> {
    let mut got = 0usize;
    while got < buf.len() {
        match file.read_at(&mut buf[got..], start + got as u64) {
            Ok(0) => break,
            Ok(r) => got += r,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(got)
}

struct ReadCtx {
    buf: AlignedBuf,
    dctx: zstd_safe::DCtx<'static>,
    scratch: Vec<u8>,
}

thread_local! {
    static READ_CTX: RefCell<ReadCtx> = RefCell::new(ReadCtx {
        buf: AlignedBuf::new(64 << 10),
        dctx: zstd_safe::DCtx::create(),
        scratch: Vec::with_capacity(32 << 10),
    });
}

/// Whether page files are opened with `O_DIRECT` (`ARCHIVE_V4_DIRECT_IO=0`
/// turns it off). The store keeps its own page cache, so going through the
/// kernel's would hold every page twice.
fn direct_io() -> bool {
    std::env::var("ARCHIVE_V4_DIRECT_IO").map_or(true, |v| v.trim() != "0")
}

/// Decompress a zstd frame written with `dict` (or with none) that holds at
/// most `max_raw` bytes.
pub(crate) fn decompress_frame(
    frame: &[u8],
    dict: &Dict,
    max_raw: usize,
    what: &str,
) -> Result<Vec<u8>, Error> {
    let size = match zstd_safe::get_frame_content_size(frame) {
        Ok(Some(n)) if n as usize <= max_raw => n as usize,
        _ => return Err(corrupt(format!("{what}: no valid zstd frame"))),
    };
    let mut out = Vec::with_capacity(size);
    READ_CTX.with(|ctx| {
        ctx.borrow_mut()
            .dctx
            .decompress_using_ddict(&mut out, frame, dict.dd.as_ddict())
            .map_err(|code| {
                corrupt(format!(
                    "{what} does not decompress: {}",
                    zstd_safe::get_error_name(code)
                ))
            })
    })?;
    if out.len() != size {
        return Err(corrupt(format!("{what} decompressed to the wrong size")));
    }
    Ok(out)
}

pub(crate) fn data_path(base: &Path) -> PathBuf {
    with_ext(base, "v4")
}
pub(crate) fn index_path(base: &Path) -> PathBuf {
    with_ext(base, "cidx")
}
pub(crate) fn bloom_path(base: &Path) -> PathBuf {
    with_ext(base, "bloom")
}

fn with_ext(base: &Path, ext: &str) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Write `bytes` to `path` and make them durable.
pub(crate) fn write_file_synced(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut f = File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

pub(crate) fn sync_dir(dir: &Path) -> Result<(), Error> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// An immutable page file, open for reads.
pub(crate) struct PageFile {
    pub(crate) fid: u32,
    pub(crate) storage: bool,
    /// Path without extension.
    pub(crate) base: PathBuf,
    file: File,
    direct: bool,
    pub(crate) index: Index,
    pub(crate) dict: Arc<Dict>,
}

impl PageFile {
    pub(crate) fn open(
        base: &Path,
        fid: u32,
        storage: bool,
        dict: Arc<Dict>,
    ) -> Result<Self, Error> {
        let index = Index::parse(&std::fs::read(index_path(base))?)?;
        if index.key_len() != key_len(storage) {
            return Err(corrupt(format!("{base:?}: index of the wrong kind")));
        }
        let path = data_path(base);
        let mut direct = direct_io();
        let file = if direct {
            match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECT)
                .open(&path)
            {
                Ok(f) => f,
                // The filesystem has no direct IO (tmpfs).
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                    direct = false;
                    File::open(&path)?
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            File::open(&path)?
        };
        let len = file.metadata()?.len();
        if len != index.data_len() {
            return Err(corrupt(format!(
                "{path:?}: {len} bytes, its index describes {}",
                index.data_len()
            )));
        }
        Ok(Self {
            fid,
            storage,
            base: base.to_path_buf(),
            file,
            direct,
            index,
            dict,
        })
    }

    pub(crate) fn data_len(&self) -> u64 {
        self.index.data_len()
    }

    /// Read `[off, end)` into `buf` (aligned if the file is direct) and
    /// return where `off` lies in it.
    fn read_range<'a>(
        &self,
        buf: &'a mut AlignedBuf,
        off: u64,
        end: u64,
    ) -> Result<&'a [u8], Error> {
        let (start, aend) = if self.direct {
            (
                off & !(ALIGN as u64 - 1),
                (end + ALIGN as u64 - 1) & !(ALIGN as u64 - 1),
            )
        } else {
            (off, end)
        };
        let b = buf.slice((aend - start) as usize);
        let got = read_at(&self.file, b, start)?;
        if (got as u64) < end - start {
            return Err(corrupt(format!("{:?}: short read at {off}", self.base)));
        }
        Ok(&b[(off - start) as usize..(end - start) as usize])
    }

    /// Read and decompress page `c` from the device.
    pub(crate) fn read_page(&self, c: usize) -> Result<Arc<[u8]>, Error> {
        let (off, end) = self.index.range(c);
        READ_CTX.with(|ctx| {
            let ctx = &mut *ctx.borrow_mut();
            let start = std::time::Instant::now();
            let stored = self.read_range(&mut ctx.buf, off, end)?;
            io_stats().note_read(end - off, start.elapsed());
            decode_stored(&mut ctx.dctx, &self.dict, stored, &mut ctx.scratch)
                .map_err(|e| corrupt(format!("{:?} page {c}: {e}", self.base)))?;
            Ok(Arc::from(ctx.scratch.as_slice()))
        })
    }
}

impl std::fmt::Debug for PageFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageFile")
            .field("fid", &self.fid)
            .field("base", &self.base)
            .finish()
    }
}

/// Reads a page file front to back in large windows, for merges, scans and
/// rewrites. It does not go through the page cache.
pub(crate) struct SeqReader {
    file: Arc<PageFile>,
    buf: AlignedBuf,
    wstart: u64,
    wlen: usize,
    dctx: zstd_safe::DCtx<'static>,
    scratch: Vec<u8>,
    /// Bytes read from the device so far.
    pub(crate) io_bytes: u64,
}

impl SeqReader {
    pub(crate) fn new(file: Arc<PageFile>) -> Self {
        Self {
            file,
            buf: AlignedBuf::new(ALIGN),
            wstart: 0,
            wlen: 0,
            dctx: zstd_safe::DCtx::create(),
            scratch: Vec::new(),
            io_bytes: 0,
        }
    }

    /// The stored bytes of page `c`.
    pub(crate) fn stored(&mut self, c: usize) -> Result<&[u8], Error> {
        let (off, end) = self.file.index.range(c);
        if off < self.wstart || end > self.wstart + self.wlen as u64 {
            let flen = self.file.data_len();
            self.wstart = off & !(ALIGN as u64 - 1);
            let want = ((flen - self.wstart) as usize)
                .min(SEQ_WINDOW)
                .max((end - self.wstart) as usize)
                .div_ceil(ALIGN)
                * ALIGN;
            let b = self.buf.slice(want);
            self.wlen = read_at(&self.file.file, b, self.wstart)?;
            self.io_bytes += self.wlen as u64;
            if end > self.wstart + self.wlen as u64 {
                return Err(corrupt(format!(
                    "{:?}: short read at {off}",
                    self.file.base
                )));
            }
        }
        let s = (off - self.wstart) as usize;
        Ok(&self.buf.slice(self.wlen)[s..s + (end - off) as usize])
    }

    /// Page `c`, decompressed.
    pub(crate) fn page(&mut self, c: usize) -> Result<&[u8], Error> {
        let (off, end) = self.file.index.range(c);
        self.stored(c)?;
        let s = (off - self.wstart) as usize;
        let stored = &self.buf.slice(self.wlen)[s..s + (end - off) as usize];
        let (dict, base) = (&self.file.dict, &self.file.base);
        decode_stored(&mut self.dctx, dict, stored, &mut self.scratch)
            .map_err(|e| corrupt(format!("{base:?} page {c}: {e}")))?;
        Ok(&self.scratch)
    }
}

/// One version, owned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rec {
    pub(crate) key: [u8; 64],
    pub(crate) block: u64,
    pub(crate) val: Val,
}

/// The versions of a page file in order, optionally only those of keys in
/// `[lo, hi)`, with a block in `[from, to]`.
pub(crate) struct Cursor {
    seq: SeqReader,
    storage: bool,
    next_page: usize,
    end_page: usize,
    buf: Vec<Rec>,
    pos: usize,
    lo: Option<[u8; 64]>,
    hi: Option<[u8; 64]>,
    from: u64,
    to: u64,
}

impl Cursor {
    pub(crate) fn new(
        file: Arc<PageFile>,
        lo: Option<[u8; 64]>,
        hi: Option<[u8; 64]>,
        from: u64,
        to: u64,
    ) -> Self {
        let storage = file.storage;
        let k = key_len(storage);
        let n = file.index.pages();
        // The index places a key only up to the pages sharing its window:
        // start at the first of them, end after the last.
        let start = lo
            .as_ref()
            .and_then(|lo| file.index.floor_range(&lo[..k], 0))
            .map_or(0, |r| r.0);
        let end = match &hi {
            // The page holding the floor of `hi` may still hold smaller keys.
            Some(hi) => file
                .index
                .floor_range(&hi[..k], 0)
                .map_or(0, |r| r.1 + 1)
                .min(n),
            None => n,
        };
        Self {
            seq: SeqReader::new(file),
            storage,
            next_page: start,
            end_page: end,
            buf: Vec::new(),
            pos: 0,
            lo,
            hi,
            from,
            to,
        }
    }

    /// Bytes read from the device so far.
    pub(crate) fn io_bytes(&self) -> u64 {
        self.seq.io_bytes
    }

    pub(crate) fn peek(&mut self) -> Result<Option<&Rec>, Error> {
        while self.pos >= self.buf.len() {
            if self.next_page >= self.end_page {
                return Ok(None);
            }
            let c = self.next_page;
            self.next_page += 1;
            self.buf.clear();
            self.pos = 0;
            let (storage, from, to) = (self.storage, self.from, self.to);
            let k = key_len(storage);
            let (lo, hi) = (&self.lo, &self.hi);
            let buf = &mut self.buf;
            let page = self.seq.page(c)?;
            codec::for_each(page, storage, |key, block, val| {
                if block >= from
                    && block <= to
                    && lo.as_ref().is_none_or(|lo| key >= &lo[..k])
                    && hi.as_ref().is_none_or(|hi| key < &hi[..k])
                {
                    let mut kk = [0u8; 64];
                    kk[..k].copy_from_slice(key);
                    buf.push(Rec {
                        key: kk,
                        block,
                        val,
                    });
                }
            })?;
        }
        Ok(Some(&self.buf[self.pos]))
    }

    pub(crate) fn next(&mut self) -> Result<Option<Rec>, Error> {
        if self.peek()?.is_none() {
            return Ok(None);
        }
        self.pos += 1;
        Ok(Some(self.buf[self.pos - 1]))
    }
}

/// K-way merge of cursors in (key, block) order. Inputs may repeat a
/// version (a segment that overlaps a file already merged); the later input
/// wins and the repeats are dropped.
pub(crate) struct MergeIter {
    inputs: Vec<Cursor>,
    heap: BinaryHeap<std::cmp::Reverse<([u8; 64], u64, usize)>>,
}

impl MergeIter {
    pub(crate) fn new(mut inputs: Vec<Cursor>) -> Result<Self, Error> {
        let mut heap = BinaryHeap::new();
        for (i, c) in inputs.iter_mut().enumerate() {
            if let Some(r) = c.peek()? {
                heap.push(std::cmp::Reverse((r.key, r.block, i)));
            }
        }
        Ok(Self { inputs, heap })
    }

    pub(crate) fn io_bytes(&self) -> u64 {
        self.inputs.iter().map(Cursor::io_bytes).sum()
    }

    /// Key and block of the next version.
    pub(crate) fn peek(&self) -> Option<([u8; 64], u64)> {
        self.heap
            .peek()
            .map(|std::cmp::Reverse((k, b, _))| (*k, *b))
    }

    fn pop(&mut self) -> Result<Option<Rec>, Error> {
        let Some(std::cmp::Reverse((_, _, i))) = self.heap.pop() else {
            return Ok(None);
        };
        let r = self.inputs[i].next()?.expect("peeked");
        if let Some(n) = self.inputs[i].peek()? {
            self.heap.push(std::cmp::Reverse((n.key, n.block, i)));
        }
        Ok(Some(r))
    }

    pub(crate) fn next(&mut self) -> Result<Option<Rec>, Error> {
        let Some(mut r) = self.pop()? else {
            return Ok(None);
        };
        // The heap orders equal (key, block) by input, so the last one popped
        // is the latest input's.
        while self.peek() == Some((r.key, r.block)) {
            r = self.pop()?.expect("peeked");
        }
        Ok(Some(r))
    }
}

// ---------------------------------------------------------------------------
// blocked bloom filter: one 512-bit block per key, 7 probes, ~10 bits/key
// ---------------------------------------------------------------------------

pub(crate) struct Bloom {
    bits: Vec<u64>,
    nblocks: u64,
}

/// Two hash words of a key. Keys are keccak hashes, so their own bytes
/// serve; a storage key mixes in the address, since slot hashes repeat
/// across contracts.
#[inline]
pub(crate) fn key_hashes(key: &[u8]) -> (u64, u64) {
    let n = key.len();
    let a = u64::from_le_bytes(key[n - 8..n].try_into().unwrap());
    let b = u64::from_le_bytes(key[n - 16..n - 8].try_into().unwrap());
    let (c, d) = if n == 64 {
        (
            u64::from_le_bytes(key[24..32].try_into().unwrap()),
            u64::from_le_bytes(key[16..24].try_into().unwrap()),
        )
    } else {
        (0, 0)
    };
    (a ^ c.rotate_left(17), b ^ d.rotate_left(31))
}

impl Bloom {
    pub(crate) fn new(keys: u64) -> Self {
        let nblocks = (keys.saturating_mul(10)).div_ceil(512).max(1);
        Self {
            bits: vec![0; (nblocks * 8) as usize],
            nblocks,
        }
    }

    pub(crate) fn insert(&mut self, key: &[u8]) {
        let (h1, h2) = key_hashes(key);
        self.insert_hashes(h1, h2);
    }

    pub(crate) fn insert_hashes(&mut self, h1: u64, mut h2: u64) {
        let b = ((h1 % self.nblocks) * 8) as usize;
        for _ in 0..7 {
            let bit = (h2 & 511) as usize;
            self.bits[b + bit / 64] |= 1 << (bit % 64);
            h2 >>= 9;
        }
    }

    #[inline]
    pub(crate) fn may_contain(&self, key: &[u8]) -> bool {
        let (h1, mut h2) = key_hashes(key);
        let b = ((h1 % self.nblocks) * 8) as usize;
        for _ in 0..7 {
            let bit = (h2 & 511) as usize;
            if self.bits[b + bit / 64] & (1 << (bit % 64)) == 0 {
                return false;
            }
            h2 >>= 9;
        }
        true
    }

    pub(crate) fn mem_bytes(&self) -> u64 {
        self.bits.len() as u64 * 8
    }

    /// `nblocks u64 | bits | crc32`, little-endian.
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.bits.len() * 8);
        out.extend_from_slice(&self.nblocks.to_le_bytes());
        for x in &self.bits {
            out.extend_from_slice(&x.to_le_bytes());
        }
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    pub(crate) fn from_bytes(b: &[u8]) -> Result<Self, Error> {
        let bad = || corrupt("malformed filter");
        if b.len() < 8 {
            return Err(bad());
        }
        let nblocks = rd64(b, 0);
        let body = nblocks
            .checked_mul(64)
            .and_then(|n| n.checked_add(8))
            .ok_or_else(bad)? as usize;
        if nblocks == 0 {
            return Err(bad());
        }
        if b.len() == body + 4 {
            if crc32fast::hash(&b[..body]) != rd32(b, body) {
                return Err(corrupt("filter checksum mismatch"));
            }
        } else if b.len() != body {
            // (The prototype wrote no checksum.)
            return Err(bad());
        }
        let bits = b[8..body]
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        Ok(Self { bits, nblocks })
    }

    pub(crate) fn load(path: &Path) -> Result<Self, Error> {
        Self::from_bytes(&std::fs::read(path)?)
    }
}

// ---------------------------------------------------------------------------
// writer
// ---------------------------------------------------------------------------

pub(crate) struct WriterOut {
    pub(crate) records: u64,
    pub(crate) bytes: u64,
    pub(crate) distinct: u64,
    pub(crate) bloom: Option<Bloom>,
    pub(crate) first_key: Option<[u8; 64]>,
}

/// Writes one page file. Nothing is visible to readers until the caller
/// lists the file in the manifest.
pub(crate) struct PageWriter {
    base: PathBuf,
    k: usize,
    data: BufWriter<File>,
    comp: zstd::bulk::Compressor<'static>,
    enc: PageEnc,
    raw: Vec<u8>,
    frame: Vec<u8>,
    offset: u64,
    synced: u64,
    index: IndexBuilder,
    records: u64,
    copied: u64,
    bloom: Option<Bloom>,
    last: [u8; 64],
    have_last: bool,
    first_key: Option<[u8; 64]>,
    distinct: u64,
}

impl PageWriter {
    /// `bloom_keys`: build a filter of the keys, sized for that many.
    pub(crate) fn new(
        base: &Path,
        storage: bool,
        dict: &Dict,
        level: i32,
        bloom_keys: Option<u64>,
    ) -> Result<Self, Error> {
        let mut comp = zstd::bulk::Compressor::with_dictionary(level, &dict.raw)?;
        comp.set_parameter(zstd_safe::CParameter::ChecksumFlag(true))?;
        Ok(Self {
            base: base.to_path_buf(),
            k: key_len(storage),
            data: BufWriter::with_capacity(1 << 20, File::create(data_path(base))?),
            comp,
            enc: PageEnc::new(storage),
            raw: Vec::new(),
            frame: Vec::new(),
            offset: 0,
            synced: 0,
            index: IndexBuilder::new(key_len(storage)),
            records: 0,
            copied: 0,
            bloom: bloom_keys.map(Bloom::new),
            last: [0; 64],
            have_last: false,
            first_key: None,
            distinct: 0,
        })
    }

    /// Bytes written so far, the open page aside.
    pub(crate) fn bytes(&self) -> u64 {
        self.offset
    }

    /// Whether the open page is short enough that the next stored page
    /// should be merged into it instead of copied after it.
    pub(crate) fn open_page_is_short(&self) -> bool {
        self.enc.len() > 0 && self.enc.size() < PAGE_TARGET / 2
    }

    /// Whether the file may end before `key`: the last record is known (it
    /// was added, not copied in a page) and is of another key. A key's
    /// versions never span files.
    pub(crate) fn can_cut_before(&self, key: &[u8]) -> bool {
        self.have_last && self.last[..self.k] != *key
    }

    fn note_first(&mut self, key: &[u8]) {
        if self.first_key.is_none() {
            let mut k = [0u8; 64];
            k[..self.k].copy_from_slice(key);
            self.first_key = Some(k);
        }
    }

    pub(crate) fn add(&mut self, key: &[u8], block: u64, val: &Val) -> Result<(), Error> {
        if self.enc.len() > 0 && self.enc.size() >= PAGE_TARGET {
            self.flush_page()?;
        }
        if self.enc.len() == 0 {
            self.index.push(key, block, self.offset);
            self.note_first(key);
        }
        if !self.have_last || self.last[..self.k] != *key {
            self.distinct += 1;
            if let Some(b) = &mut self.bloom {
                b.insert(key);
            }
            self.last[..self.k].copy_from_slice(key);
            self.have_last = true;
        }
        self.enc.add(key, block, val);
        self.records += 1;
        Ok(())
    }

    fn flush_page(&mut self) -> Result<(), Error> {
        self.enc.finish(&mut self.raw);
        self.frame.clear();
        self.frame
            .reserve(zstd_safe::compress_bound(self.raw.len()));
        self.comp.compress_to_buffer(&self.raw, &mut self.frame)?;
        self.data
            .write_all(&(self.frame.len() as u32).to_le_bytes())?;
        self.data.write_all(&self.frame)?;
        let len = 4 + self.frame.len() as u64;
        let padded = len.div_ceil(UNIT) * UNIT;
        self.data
            .write_all(&[0u8; UNIT as usize][..(padded - len) as usize])?;
        self.offset += padded;
        self.relieve_page_cache()
    }

    /// Append a page as stored in another file with this writer's
    /// dictionary: `stored` is its bytes on disk, length header and padding
    /// included, `records` its record count and `(first_key, first_block)`
    /// its first record. The page before it is closed.
    pub(crate) fn copy_page(
        &mut self,
        first_key: &[u8],
        first_block: u64,
        stored: &[u8],
        records: u64,
    ) -> Result<(), Error> {
        if self.enc.len() > 0 {
            self.flush_page()?;
        }
        debug_assert_eq!(stored.len() as u64 % UNIT, 0);
        self.index.push(first_key, first_block, self.offset);
        self.note_first(first_key);
        self.data.write_all(stored)?;
        self.offset += stored.len() as u64;
        self.records += records;
        self.copied += 1;
        // The copied page's keys are unknown here: the next key added counts
        // as new, and callers do not ask for a filter or a distinct count.
        self.have_last = false;
        self.relieve_page_cache()
    }

    fn relieve_page_cache(&mut self) -> Result<(), Error> {
        if self.offset - self.synced < WRITE_SYNC_BYTES {
            return Ok(());
        }
        self.data.flush()?;
        let f = self.data.get_ref();
        f.sync_data()?;
        // Advice only.
        unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        self.synced = self.offset;
        Ok(())
    }

    /// Close the file and make it durable, with its index and filter.
    pub(crate) fn finish(mut self) -> Result<WriterOut, Error> {
        if self.enc.len() > 0 {
            self.flush_page()?;
        }
        self.data.flush()?;
        let f = self.data.get_ref();
        f.sync_all()?;
        unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        write_file_synced(&index_path(&self.base), &self.index.finish(self.offset)?)?;
        if let Some(b) = &self.bloom {
            write_file_synced(&bloom_path(&self.base), &b.to_bytes())?;
        }
        Ok(WriterOut {
            records: self.records,
            bytes: self.offset,
            distinct: self.distinct,
            bloom: self.bloom,
            first_key: self.first_key,
        })
    }
}

/// Remove a page file's data, index and filter.
pub(crate) fn remove_page_file(base: &Path) {
    for path in [data_path(base), index_path(base), bloom_path(base)] {
        let _ = std::fs::remove_file(path);
    }
}

/// Train a dictionary on raw pages built from `recs` (sorted versions of
/// one kind). Too few samples for zstd's trainer yield an empty dictionary,
/// which compresses as none.
pub(crate) fn train_dict<'a>(storage: bool, samples: impl Iterator<Item = &'a [Rec]>) -> Vec<u8> {
    let k = key_len(storage);
    let mut pages: Vec<Vec<u8>> = Vec::new();
    let mut enc = PageEnc::new(storage);
    for recs in samples {
        for r in recs {
            if enc.len() > 0 && enc.size() >= PAGE_TARGET {
                let mut page = Vec::new();
                enc.finish(&mut page);
                pages.push(page);
            }
            enc.add(&r.key[..k], r.block, &r.val);
        }
        if enc.len() > 0 {
            let mut page = Vec::new();
            enc.finish(&mut page);
            pages.push(page);
        }
    }
    let total: usize = pages.iter().map(Vec::len).sum();
    if pages.len() < 8 || total < 64 << 10 {
        return Vec::new();
    }
    zstd::dict::from_samples(&pages, DICT_BYTES.min(total / 8)).unwrap_or_default()
}

/// Size of a trained dictionary.
const DICT_BYTES: usize = 112 << 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bloom_round_trip() {
        let mut b = Bloom::new(1000);
        let key = |i: u32| {
            let mut k = [0u8; 64];
            for (j, c) in k.chunks_mut(4).enumerate() {
                c.copy_from_slice(
                    &(i.wrapping_mul(2654435761).rotate_left(j as u32) ^ j as u32).to_le_bytes(),
                );
            }
            k
        };
        for i in 0..1000 {
            b.insert(&key(i));
        }
        let b = Bloom::from_bytes(&b.to_bytes()).unwrap();
        assert!((0..1000).all(|i| b.may_contain(&key(i))));
        let fp = (1000..21000).filter(|i| b.may_contain(&key(*i))).count();
        assert!(fp < 600, "false positives {fp} of 20000");
        let mut bytes = b.to_bytes();
        bytes[20] ^= 4;
        assert!(Bloom::from_bytes(&bytes).is_err());
    }
}
