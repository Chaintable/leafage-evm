//! Archive schema v3 sealed history segments: immutable files of account or
//! storage versions answering "greatest version `<= h` of a key" with one
//! descent through a static fence tree. See `docs/ArchiveSchemaV3.md` for the
//! semantics and the byte layout.

use memmap2::{Advice, Mmap};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Page size of data and fence pages.
pub const PAGE: usize = 4096;
const MAGIC: [u8; 8] = *b"LFV3SEG\0";
const FORMAT_VERSION: u32 = 1;
/// `n_keys`, `n_records`, reserved.
const DATA_HEADER: usize = 8;
/// `first_entry`, `n_entries`.
const FENCE_HEADER: usize = 10;
const MAX_LEVELS: usize = 8;
/// Record key = `key ‖ be64(block)`; storage keys are the longest.
const MAX_RECORD_KEY: usize = 64 + 8;

/// Which state a segment holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum SegKind {
    Account = 1,
    Storage = 2,
}

impl SegKind {
    /// Length of the segment's keys.
    pub const fn key_len(self) -> usize {
        match self {
            SegKind::Account => 32,
            SegKind::Storage => 64,
        }
    }

    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(SegKind::Account),
            2 => Some(SegKind::Storage),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            SegKind::Account => "account",
            SegKind::Storage => "storage",
        }
    }
}

#[inline]
fn rd16(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes([b[at], b[at + 1]]) as usize
}

#[inline]
fn rd64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// The record key `key ‖ be64(block)` in a stack buffer.
#[inline]
fn record_key(key: &[u8], block: u64) -> ([u8; MAX_RECORD_KEY], usize) {
    let mut buf = [0u8; MAX_RECORD_KEY];
    buf[..key.len()].copy_from_slice(key);
    buf[key.len()..key.len() + 8].copy_from_slice(&block.to_be_bytes());
    (buf, key.len() + 8)
}

/// Shortest prefix of `b` that sorts after `a` (`a < b`).
fn separator<'b>(a: &[u8], b: &'b [u8]) -> &'b [u8] {
    debug_assert!(a < b);
    let common = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    &b[..(common + 1).min(b.len())]
}

/// Minimal big-endian form of a 32-byte storage value: leading zeros
/// stripped, zero is empty.
pub fn strip_leading_zeros(value: &[u8]) -> &[u8] {
    let start = value.iter().position(|&b| b != 0).unwrap_or(value.len());
    &value[start..]
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Packs fence entries (separators) into fence pages.
struct FencePacker {
    page: Vec<u8>,
    ends: Vec<u16>,
    bytes: Vec<u8>,
    first_entry: u64,
    entries: u64,
}

impl FencePacker {
    fn new() -> Self {
        Self {
            page: vec![0u8; PAGE],
            ends: Vec::new(),
            bytes: Vec::new(),
            first_entry: 0,
            entries: 0,
        }
    }

    fn fits(&self, len: usize) -> bool {
        FENCE_HEADER + 2 * (self.ends.len() + 1) + self.bytes.len() + len <= PAGE
    }

    /// Serialize the current page and reset; returns whether one was written.
    fn flush(&mut self, out: &mut impl Write) -> io::Result<bool> {
        if self.ends.is_empty() {
            return Ok(false);
        }
        let n = self.ends.len();
        let base = FENCE_HEADER + 2 * n;
        self.page.fill(0);
        self.page[0..8].copy_from_slice(&self.first_entry.to_le_bytes());
        self.page[8..10].copy_from_slice(&(n as u16).to_le_bytes());
        for (i, end) in self.ends.iter().enumerate() {
            let abs = (base + *end as usize) as u16;
            self.page[FENCE_HEADER + 2 * i..FENCE_HEADER + 2 * i + 2]
                .copy_from_slice(&abs.to_le_bytes());
        }
        self.page[base..base + self.bytes.len()].copy_from_slice(&self.bytes);
        out.write_all(&self.page)?;
        self.first_entry += n as u64;
        self.ends.clear();
        self.bytes.clear();
        Ok(true)
    }

    /// Add an entry; `Some(first entry)` of a new page when it starts one.
    fn push(&mut self, entry: &[u8], out: &mut impl Write) -> io::Result<bool> {
        let mut new_page = self.ends.is_empty();
        if !self.fits(entry.len()) {
            self.flush(out)?;
            new_page = true;
        }
        self.bytes.extend_from_slice(entry);
        self.ends.push(self.bytes.len() as u16);
        self.entries += 1;
        Ok(new_page)
    }
}

/// Writes one segment. Records must be pushed in strictly ascending
/// `(key, block)` order.
pub struct SegmentWriter {
    out: BufWriter<File>,
    path: PathBuf,
    kind: SegKind,
    key_len: usize,
    shard: u16,
    epoch_start: u64,
    epoch_end: u64,
    // Current data page.
    keys: Vec<u8>,
    first: Vec<u16>,
    blocks: Vec<u64>,
    value_ends: Vec<u16>,
    values: Vec<u8>,
    used: usize,
    page_buf: Vec<u8>,
    // Level-0 separators, one per data page.
    sep_bytes: Vec<u8>,
    sep_ends: Vec<u64>,
    last_record: Vec<u8>,
    has_last: bool,
    data_pages: u64,
    records: u64,
    distinct_keys: u64,
}

impl SegmentWriter {
    pub fn create(
        path: impl AsRef<Path>,
        kind: SegKind,
        shard: u16,
        epoch_start: u64,
        epoch_end: u64,
    ) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::create_new(&path)?;
        let mut out = BufWriter::with_capacity(8 << 20, file);
        out.write_all(&[0u8; PAGE])?; // header, rewritten by finish()
        Ok(Self {
            out,
            path,
            kind,
            key_len: kind.key_len(),
            shard,
            epoch_start,
            epoch_end,
            keys: Vec::new(),
            first: Vec::new(),
            blocks: Vec::new(),
            value_ends: Vec::new(),
            values: Vec::new(),
            used: DATA_HEADER,
            page_buf: vec![0u8; PAGE],
            sep_bytes: Vec::new(),
            sep_ends: Vec::new(),
            last_record: Vec::with_capacity(MAX_RECORD_KEY),
            has_last: false,
            data_pages: 0,
            records: 0,
            distinct_keys: 0,
        })
    }

    /// Largest value a record may carry.
    pub fn max_value_len(&self) -> usize {
        PAGE - DATA_HEADER - (self.key_len + 2) - 10
    }

    pub fn push(&mut self, key: &[u8], block: u64, value: &[u8]) -> io::Result<()> {
        assert_eq!(key.len(), self.key_len, "segment key length");
        if value.len() > self.max_value_len() {
            return Err(invalid(format!("value of {} bytes too large", value.len())));
        }
        let (rk, rlen) = record_key(key, block);
        let rk = &rk[..rlen];
        if self.has_last && rk <= self.last_record.as_slice() {
            return Err(invalid("records out of order"));
        }
        let new_key_in_page =
            |w: &Self| w.first.is_empty() || &w.keys[w.keys.len() - w.key_len..] != key;
        let need = |w: &Self| 10 + value.len() + if new_key_in_page(w) { w.key_len + 2 } else { 0 };
        if !self.first.is_empty() && self.used + need(self) > PAGE {
            self.flush_page()?;
        }
        if self.blocks.is_empty() {
            // First record of a data page: its level-0 separator.
            let sep: &[u8] = if self.has_last {
                separator(&self.last_record, rk)
            } else {
                &[]
            };
            self.sep_bytes.extend_from_slice(sep);
            self.sep_ends.push(self.sep_bytes.len() as u64);
        }
        let is_new_key = new_key_in_page(self);
        if is_new_key {
            let continues = self.has_last && &self.last_record[..self.key_len] == key;
            if !continues {
                self.distinct_keys += 1;
            }
            self.keys.extend_from_slice(key);
            self.first.push(self.blocks.len() as u16);
            self.used += self.key_len + 2;
        }
        self.blocks.push(block);
        self.values.extend_from_slice(value);
        self.value_ends.push(self.values.len() as u16);
        self.used += 10 + value.len();
        self.last_record.clear();
        self.last_record.extend_from_slice(rk);
        self.has_last = true;
        self.records += 1;
        Ok(())
    }

    fn flush_page(&mut self) -> io::Result<()> {
        if self.blocks.is_empty() {
            return Ok(());
        }
        let nk = self.first.len();
        let nr = self.blocks.len();
        let p = &mut self.page_buf;
        p.fill(0);
        p[0..2].copy_from_slice(&(nk as u16).to_le_bytes());
        p[2..4].copy_from_slice(&(nr as u16).to_le_bytes());
        let mut at = DATA_HEADER;
        p[at..at + self.keys.len()].copy_from_slice(&self.keys);
        at += self.keys.len();
        for f in &self.first {
            p[at..at + 2].copy_from_slice(&f.to_le_bytes());
            at += 2;
        }
        for b in &self.blocks {
            p[at..at + 8].copy_from_slice(&b.to_le_bytes());
            at += 8;
        }
        let values_at = at + 2 * nr;
        for end in &self.value_ends {
            let abs = (values_at + *end as usize) as u16;
            p[at..at + 2].copy_from_slice(&abs.to_le_bytes());
            at += 2;
        }
        p[values_at..values_at + self.values.len()].copy_from_slice(&self.values);
        debug_assert!(values_at + self.values.len() <= PAGE);
        self.out.write_all(p)?;
        self.data_pages += 1;
        self.keys.clear();
        self.first.clear();
        self.blocks.clear();
        self.value_ends.clear();
        self.values.clear();
        self.used = DATA_HEADER;
        Ok(())
    }

    /// Write the fence levels and the header. Returns the file's stats.
    pub fn finish(mut self) -> io::Result<SegmentStats> {
        self.flush_page()?;
        let mut levels: Vec<(u64, u64, u64)> = Vec::new();
        let mut next_page = 1 + self.data_pages;
        if self.data_pages > 0 {
            // Level 0 from the separators, then one level per fan-out until a
            // single page remains.
            let mut firsts: Vec<Vec<u8>> = Vec::new();
            let mut packer = FencePacker::new();
            let mut start = 0usize;
            for &end in &self.sep_ends {
                let entry = &self.sep_bytes[start..end as usize];
                if packer.push(entry, &mut self.out)? {
                    firsts.push(entry.to_vec());
                }
                start = end as usize;
            }
            packer.flush(&mut self.out)?;
            let mut pages = firsts.len() as u64;
            levels.push((next_page, pages, packer.entries));
            next_page += pages;
            self.sep_bytes = Vec::new();
            self.sep_ends = Vec::new();
            while pages > 1 {
                if levels.len() == MAX_LEVELS {
                    return Err(invalid("too many fence levels"));
                }
                let mut packer = FencePacker::new();
                let mut next_firsts = Vec::new();
                for entry in &firsts {
                    if packer.push(entry, &mut self.out)? {
                        next_firsts.push(entry.clone());
                    }
                }
                packer.flush(&mut self.out)?;
                pages = next_firsts.len() as u64;
                levels.push((next_page, pages, packer.entries));
                next_page += pages;
                firsts = next_firsts;
            }
        }
        let mut header = Vec::with_capacity(256);
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        header.push(self.kind as u8);
        header.push(self.key_len as u8);
        header.extend_from_slice(&self.shard.to_le_bytes());
        header.extend_from_slice(&self.epoch_start.to_le_bytes());
        header.extend_from_slice(&self.epoch_end.to_le_bytes());
        header.extend_from_slice(&self.records.to_le_bytes());
        header.extend_from_slice(&self.distinct_keys.to_le_bytes());
        header.extend_from_slice(&self.data_pages.to_le_bytes());
        header.push(levels.len() as u8);
        for (first, pages, entries) in &levels {
            header.extend_from_slice(&first.to_le_bytes());
            header.extend_from_slice(&pages.to_le_bytes());
            header.extend_from_slice(&entries.to_le_bytes());
        }
        let crc = crc32fast::hash(&header);
        header.extend_from_slice(&crc.to_le_bytes());
        self.out.flush()?;
        let mut file = self.out.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.sync_all()?;
        Ok(SegmentStats {
            path: self.path,
            bytes: next_page * PAGE as u64,
            records: self.records,
            keys: self.distinct_keys,
            data_pages: self.data_pages,
            fence_levels: levels.len(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct SegmentStats {
    pub path: PathBuf,
    pub bytes: u64,
    pub records: u64,
    pub keys: u64,
    pub data_pages: u64,
    pub fence_levels: usize,
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// A version found by [`Segment::lookup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version<'a> {
    /// Block that wrote the value.
    pub block: u64,
    /// Encoded value (storage: minimal big-endian; account: slim RLP, empty
    /// when deleted).
    pub value: &'a [u8],
}

/// Result of [`Segment::lookup`] at height `h`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Read<'a> {
    /// The greatest version `<= h`, if any.
    pub version: Option<Version<'a>>,
    /// Block of the key's first version above `h` in this segment, if any:
    /// what `version` (or the absence of one) holds until.
    pub next: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Level {
    first_page: u64,
    pages: u64,
}

/// A memory-mapped segment.
pub struct Segment {
    map: Mmap,
    pub kind: SegKind,
    key_len: usize,
    pub shard: u16,
    pub epoch_start: u64,
    pub epoch_end: u64,
    pub records: u64,
    pub keys: u64,
    data_pages: u64,
    /// Bottom (level 0) first.
    levels: Vec<Level>,
}

impl std::fmt::Debug for Segment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segment")
            .field("kind", &self.kind)
            .field("shard", &self.shard)
            .field("records", &self.records)
            .field("data_pages", &self.data_pages)
            .field("levels", &self.levels.len())
            .finish()
    }
}

/// View of one data page.
struct DataPage<'a> {
    b: &'a [u8],
    k: usize,
    nk: usize,
    nr: usize,
    first_at: usize,
    blocks_at: usize,
    ends_at: usize,
}

impl<'a> DataPage<'a> {
    fn new(b: &'a [u8], k: usize) -> Self {
        let nk = rd16(b, 0);
        let nr = rd16(b, 2);
        let first_at = DATA_HEADER + nk * k;
        let blocks_at = first_at + 2 * nk;
        let ends_at = blocks_at + 8 * nr;
        Self {
            b,
            k,
            nk,
            nr,
            first_at,
            blocks_at,
            ends_at,
        }
    }

    #[inline]
    fn key(&self, i: usize) -> &'a [u8] {
        let at = DATA_HEADER + i * self.k;
        &self.b[at..at + self.k]
    }

    #[inline]
    fn first(&self, i: usize) -> usize {
        rd16(self.b, self.first_at + 2 * i)
    }

    /// One past the last record of key `i`.
    #[inline]
    fn key_end(&self, i: usize) -> usize {
        if i + 1 < self.nk {
            self.first(i + 1)
        } else {
            self.nr
        }
    }

    #[inline]
    fn block(&self, r: usize) -> u64 {
        rd64(self.b, self.blocks_at + 8 * r)
    }

    #[inline]
    fn value(&self, r: usize) -> &'a [u8] {
        let start = if r == 0 {
            self.ends_at + 2 * self.nr
        } else {
            rd16(self.b, self.ends_at + 2 * (r - 1))
        };
        let end = rd16(self.b, self.ends_at + 2 * r);
        &self.b[start..end]
    }

    /// Index of the greatest key `<= key`.
    fn floor_key(&self, key: &[u8]) -> Option<usize> {
        let (mut lo, mut hi) = (0usize, self.nk);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.key(mid) <= key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo.checked_sub(1)
    }

    /// The greatest record `<= (key, h)` as `(key index, record)`.
    fn floor(&self, key: &[u8], h: u64) -> Option<(usize, usize)> {
        let i = self.floor_key(key)?;
        let (start, end) = (self.first(i), self.key_end(i));
        if self.key(i) != key {
            return Some((i, end - 1));
        }
        // Greatest block <= h within the key's records.
        let (mut lo, mut hi) = (start, end);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.block(mid) <= h {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo > start {
            Some((i, lo - 1))
        } else if i > 0 {
            Some((i - 1, start - 1))
        } else {
            None
        }
    }
}

impl Segment {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len < PAGE as u64 || len % PAGE as u64 != 0 {
            return Err(invalid(format!(
                "{}: bad segment size {len}",
                path.display()
            )));
        }
        // SAFETY: segments are immutable once published; nothing truncates
        // or rewrites a file another process may have mapped.
        let map = unsafe { Mmap::map(&file)? };
        let h = &map[..PAGE];
        if h[0..8] != MAGIC {
            return Err(invalid(format!("{}: bad magic", path.display())));
        }
        let version = u32::from_le_bytes(h[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(invalid(format!(
                "{}: format version {version}",
                path.display()
            )));
        }
        let kind = SegKind::from_u8(h[12]).ok_or_else(|| invalid("bad segment kind"))?;
        let key_len = h[13] as usize;
        if key_len != kind.key_len() {
            return Err(invalid("key length does not match kind"));
        }
        let shard = u16::from_le_bytes(h[14..16].try_into().unwrap());
        let epoch_start = rd64(h, 16);
        let epoch_end = rd64(h, 24);
        let records = rd64(h, 32);
        let keys = rd64(h, 40);
        let data_pages = rd64(h, 48);
        let n_levels = h[56] as usize;
        if n_levels > MAX_LEVELS {
            return Err(invalid("too many fence levels"));
        }
        let mut levels = Vec::with_capacity(n_levels);
        let mut at = 57;
        for _ in 0..n_levels {
            levels.push(Level {
                first_page: rd64(h, at),
                pages: rd64(h, at + 8),
            });
            at += 24;
        }
        let crc = u32::from_le_bytes(h[at..at + 4].try_into().unwrap());
        if crc != crc32fast::hash(&h[..at]) {
            return Err(invalid(format!(
                "{}: header checksum mismatch",
                path.display()
            )));
        }
        let pages = len / PAGE as u64;
        let fence_end = levels
            .last()
            .map_or(1 + data_pages, |l| l.first_page + l.pages);
        if fence_end != pages || levels.last().is_some_and(|l| l.pages != 1) {
            return Err(invalid(format!("{}: truncated segment", path.display())));
        }
        if (data_pages == 0) != levels.is_empty() {
            return Err(invalid("fence levels do not match data pages"));
        }
        let advice = match std::env::var("ARCHIVE_HISTORY_MADVISE").as_deref() {
            Ok("normal") => Advice::Normal,
            Ok("willneed") => Advice::WillNeed,
            _ => Advice::Random,
        };
        map.advise(advice)?;
        Ok(Self {
            map,
            kind,
            key_len,
            shard,
            epoch_start,
            epoch_end,
            records,
            keys,
            data_pages,
            levels,
        })
    }

    #[inline]
    fn page(&self, idx: u64) -> &[u8] {
        let at = idx as usize * PAGE;
        &self.map[at..at + PAGE]
    }

    #[inline]
    fn data_page(&self, p: u64) -> DataPage<'_> {
        DataPage::new(self.page(1 + p), self.key_len)
    }

    /// Index of the data page whose range holds the greatest record
    /// `<= target`.
    fn find_data_page(&self, target: &[u8]) -> Option<u64> {
        if self.data_pages == 0 {
            return None;
        }
        let mut idx = 0u64;
        for level in self.levels.iter().rev() {
            let p = self.page(level.first_page + idx);
            idx = fence_floor(p, target);
        }
        Some(idx.min(self.data_pages - 1))
    }

    /// The record after `(p, ki, r)`, as `(page, key index, record)`.
    fn successor(&self, p: u64, ki: usize, r: usize) -> Option<(u64, usize, usize)> {
        let page = self.data_page(p);
        if r + 1 < page.nr {
            let ki = if r + 1 < page.key_end(ki) { ki } else { ki + 1 };
            Some((p, ki, r + 1))
        } else if p + 1 < self.data_pages {
            Some((p + 1, 0, 0))
        } else {
            None
        }
    }

    /// The greatest version `<= h` of `key`, and its next version.
    pub fn lookup(&self, key: &[u8], h: u64) -> Read<'_> {
        debug_assert_eq!(key.len(), self.key_len);
        let (buf, len) = record_key(key, h);
        let Some(p) = self.find_data_page(&buf[..len]) else {
            return Read::default();
        };
        // The greatest record <= (key, h), which may close the previous page.
        let floor = match self.data_page(p).floor(key, h) {
            Some((ki, r)) => Some((p, ki, r)),
            None if p > 0 => {
                let prev = self.data_page(p - 1);
                Some((p - 1, prev.nk - 1, prev.nr - 1))
            }
            None => None,
        };
        let succ = match floor {
            Some((fp, ki, r)) => self.successor(fp, ki, r),
            None => Some((0, 0, 0)), // (key, h) sorts before every record
        };
        let version = floor.and_then(|(fp, ki, r)| {
            let page = self.data_page(fp);
            (page.key(ki) == key).then(|| Version {
                block: page.block(r),
                value: page.value(r),
            })
        });
        let next = succ.and_then(|(sp, ki, r)| {
            let page = self.data_page(sp);
            (page.key(ki) == key).then(|| page.block(r))
        });
        Read { version, next }
    }

    /// Call `f` with each distinct key `>= start`, ascending, until it
    /// returns `false`.
    pub fn for_each_key_from(&self, start: &[u8], mut f: impl FnMut(&[u8]) -> bool) {
        debug_assert_eq!(start.len(), self.key_len);
        let (buf, len) = record_key(start, 0);
        let Some(mut p) = self.find_data_page(&buf[..len]) else {
            return;
        };
        let mut last: Option<&[u8]> = None;
        while p < self.data_pages {
            let page = self.data_page(p);
            for i in 0..page.nk {
                let k = page.key(i);
                if k < start || last == Some(k) {
                    continue;
                }
                last = Some(k);
                if !f(k) {
                    return;
                }
            }
            p += 1;
        }
    }
}

/// Global index of the greatest fence entry `<= target` in fence page `p`.
fn fence_floor(p: &[u8], target: &[u8]) -> u64 {
    let first = rd64(p, 0);
    let n = rd16(p, 8);
    let base = FENCE_HEADER + 2 * n;
    let entry = |i: usize| {
        let start = if i == 0 {
            base
        } else {
            rd16(p, FENCE_HEADER + 2 * (i - 1))
        };
        let end = rd16(p, FENCE_HEADER + 2 * i);
        &p[start..end]
    };
    let (mut lo, mut hi) = (0usize, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if entry(mid) <= target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    first + lo.saturating_sub(1) as u64
}

// ---------------------------------------------------------------------------
// Manifest and store
// ---------------------------------------------------------------------------

pub const MANIFEST_FILE: &str = "MANIFEST.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub file: String,
    pub bytes: u64,
    pub records: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEpoch {
    pub start: u64,
    pub end: u64,
    pub dir: String,
    pub account: Vec<ManifestFile>,
    pub storage: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub sealed_upto: u64,
    pub shard_bits: u8,
    pub epochs: Vec<ManifestEpoch>,
}

impl Manifest {
    /// Replace `dir/MANIFEST.json` atomically.
    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let tmp = dir.join(format!("{MANIFEST_FILE}.tmp"));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(self).map_err(io::Error::other)?)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, dir.join(MANIFEST_FILE))?;
        File::open(dir)?.sync_all()
    }
}

/// Shard of a key: the top `bits` bits of its first byte.
#[inline]
pub fn shard_of(key: &[u8], bits: u8) -> usize {
    if bits == 0 {
        0
    } else {
        (key[0] >> (8 - bits)) as usize
    }
}

/// One sealed epoch: its account and storage segments, by shard.
#[derive(Debug)]
pub struct Epoch {
    pub start: u64,
    pub end: u64,
    shard_bits: u8,
    account: Vec<Segment>,
    storage: Vec<Segment>,
}

impl Epoch {
    pub fn segment(&self, kind: SegKind, key: &[u8]) -> &Segment {
        let shard = shard_of(key, self.shard_bits);
        match kind {
            SegKind::Account => &self.account[shard],
            SegKind::Storage => &self.storage[shard],
        }
    }

    pub fn lookup(&self, kind: SegKind, key: &[u8], h: u64) -> Read<'_> {
        self.segment(kind, key).lookup(key, h)
    }
}

/// The sealed history of a v3 archive.
#[derive(Debug)]
pub struct HistoryStore {
    pub dir: PathBuf,
    /// Last sealed block `S`.
    pub sealed_upto: u64,
    epochs: Vec<Epoch>,
}

impl HistoryStore {
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(dir.join(MANIFEST_FILE))?)
            .map_err(|e| invalid(format!("manifest: {e}")))?;
        if manifest.schema != 3 {
            return Err(invalid(format!("manifest schema {}", manifest.schema)));
        }
        let shards = 1usize << manifest.shard_bits;
        let mut epochs = Vec::with_capacity(manifest.epochs.len());
        let mut expect_start = 0u64;
        for e in &manifest.epochs {
            if e.start != expect_start || e.end < e.start {
                return Err(invalid("epochs are not contiguous from genesis"));
            }
            expect_start = e.end + 1;
            let open_all = |files: &[ManifestFile], kind: SegKind| -> io::Result<Vec<Segment>> {
                if files.len() != shards {
                    return Err(invalid(format!(
                        "{} {} files, want {shards}",
                        files.len(),
                        kind.name()
                    )));
                }
                files
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let seg = Segment::open(dir.join(&e.dir).join(&f.file))?;
                        if seg.kind != kind
                            || seg.shard as usize != i
                            || seg.epoch_start != e.start
                            || seg.epoch_end != e.end
                        {
                            return Err(invalid(format!(
                                "{}: does not match the manifest",
                                f.file
                            )));
                        }
                        Ok(seg)
                    })
                    .collect()
            };
            epochs.push(Epoch {
                start: e.start,
                end: e.end,
                shard_bits: manifest.shard_bits,
                account: open_all(&e.account, SegKind::Account)?,
                storage: open_all(&e.storage, SegKind::Storage)?,
            });
        }
        if epochs.last().map(|e| e.end) != Some(manifest.sealed_upto) {
            return Err(invalid("sealed_upto does not match the last epoch"));
        }
        Ok(Self {
            dir,
            sealed_upto: manifest.sealed_upto,
            epochs,
        })
    }

    /// The epoch holding block `h`, `None` above the sealed boundary.
    pub fn epoch(&self, h: u64) -> Option<&Epoch> {
        if h > self.sealed_upto {
            return None;
        }
        let i = self.epochs.partition_point(|e| e.end < h);
        self.epochs.get(i)
    }

    pub fn last_epoch(&self) -> &Epoch {
        self.epochs.last().expect("a store has at least one epoch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Deterministic xorshift.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    type Oracle = BTreeMap<Vec<u8>, Vec<(u64, Vec<u8>)>>;

    /// A scratch directory removed on drop.
    struct TestDir(PathBuf);
    impl TestDir {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("leafage-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn build(kind: SegKind, seed: u64, n_keys: usize, hot: usize) -> (TestDir, Oracle) {
        let dir = TestDir::new("seg");
        let mut rng = Rng(seed);
        let k = kind.key_len();
        let mut oracle: Oracle = BTreeMap::new();
        for i in 0..n_keys {
            let mut key = vec![0u8; k];
            for b in key.iter_mut() {
                *b = rng.next() as u8;
            }
            // Some keys share a long prefix (slots of one contract).
            if i % 7 == 0 && i > 0 {
                let prev = oracle.keys().next_back().unwrap().clone();
                key[..k - 1].copy_from_slice(&prev[..k - 1]);
            }
            let n = if i < hot {
                2000 + rng.below(3000)
            } else {
                1 + rng.below(6)
            };
            let mut block = rng.below(50);
            let mut versions = Vec::new();
            for _ in 0..n {
                block += 1 + rng.below(40);
                let vlen = rng.below(if kind == SegKind::Account { 80 } else { 33 }) as usize;
                let value: Vec<u8> = (0..vlen).map(|_| rng.next() as u8).collect();
                versions.push((block, value));
            }
            oracle.insert(key, versions);
        }
        let mut w = SegmentWriter::create(dir.path().join("t.seg"), kind, 0, 0, u64::MAX).unwrap();
        for (key, versions) in &oracle {
            for (block, value) in versions {
                w.push(key, *block, value).unwrap();
            }
        }
        let stats = w.finish().unwrap();
        assert_eq!(stats.keys as usize, oracle.len());
        (dir, oracle)
    }

    fn expect(oracle: &Oracle, key: &[u8], h: u64) -> (Option<(u64, Vec<u8>)>, Option<u64>) {
        let Some(versions) = oracle.get(key) else {
            return (None, None);
        };
        let i = versions.partition_point(|(b, _)| *b <= h);
        let version = (i > 0).then(|| (versions[i - 1].0, versions[i - 1].1.clone()));
        (version, versions.get(i).map(|(b, _)| *b))
    }

    fn got(seg: &Segment, key: &[u8], h: u64) -> (Option<(u64, Vec<u8>)>, Option<u64>) {
        let r = seg.lookup(key, h);
        (r.version.map(|v| (v.block, v.value.to_vec())), r.next)
    }

    fn check(kind: SegKind, seed: u64, n_keys: usize, hot: usize) {
        let (dir, oracle) = build(kind, seed, n_keys, hot);
        let seg = Segment::open(dir.path().join("t.seg")).unwrap();
        let mut rng = Rng(seed ^ 0x9e37);
        let keys: Vec<&Vec<u8>> = oracle.keys().collect();
        for _ in 0..20_000 {
            let key = keys[rng.below(keys.len() as u64) as usize].clone();
            let versions = &oracle[&key];
            let h = match rng.below(4) {
                0 => versions[rng.below(versions.len() as u64) as usize].0,
                1 => versions[rng.below(versions.len() as u64) as usize]
                    .0
                    .saturating_sub(1),
                2 => versions.last().unwrap().0 + rng.below(100),
                _ => rng.below(versions.last().unwrap().0 + 10),
            };
            assert_eq!(
                got(&seg, &key, h),
                expect(&oracle, &key, h),
                "key {key:?} h {h}"
            );
            // A key that is absent.
            let mut other = key.clone();
            *other.last_mut().unwrap() ^= 0x55;
            if !oracle.contains_key(&other) {
                assert_eq!(seg.lookup(&other, h), Read::default());
            }
        }
        // Prefix iteration visits every key once, in order.
        let mut seen = Vec::new();
        seg.for_each_key_from(&vec![0u8; kind.key_len()], |k| {
            seen.push(k.to_vec());
            true
        });
        assert_eq!(seen, keys.into_iter().cloned().collect::<Vec<_>>());
        let start = oracle.keys().nth(oracle.len() / 2).unwrap().clone();
        let mut from = Vec::new();
        seg.for_each_key_from(&start, |k| {
            from.push(k.to_vec());
            from.len() < 10
        });
        let want: Vec<Vec<u8>> = oracle
            .range(start..)
            .take(10)
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(from, want);
    }

    #[test]
    fn storage_lookups_match_oracle() {
        check(SegKind::Storage, 1, 20_000, 5);
    }

    #[test]
    fn account_lookups_match_oracle() {
        check(SegKind::Account, 2, 30_000, 3);
    }

    #[test]
    fn hot_keys_spanning_many_pages() {
        check(SegKind::Storage, 3, 50, 50);
    }

    #[test]
    fn empty_and_single_record_segments() {
        let dir = TestDir::new("seg");
        let path = dir.path().join("e.seg");
        SegmentWriter::create(&path, SegKind::Account, 0, 0, 10)
            .unwrap()
            .finish()
            .unwrap();
        let seg = Segment::open(&path).unwrap();
        assert_eq!(seg.lookup(&[1u8; 32], 5), Read::default());

        let path = dir.path().join("one.seg");
        let mut w = SegmentWriter::create(&path, SegKind::Account, 0, 0, 10).unwrap();
        w.push(&[7u8; 32], 3, b"abc").unwrap();
        w.finish().unwrap();
        let seg = Segment::open(&path).unwrap();
        assert_eq!(got(&seg, &[7u8; 32], 2), (None, Some(3)));
        assert_eq!(got(&seg, &[7u8; 32], 3), (Some((3, b"abc".to_vec())), None));
        assert_eq!(seg.lookup(&[8u8; 32], 9), Read::default());
        assert_eq!(seg.lookup(&[6u8; 32], 9), Read::default());
    }

    #[test]
    fn rejects_out_of_order_records() {
        let dir = TestDir::new("seg");
        let mut w =
            SegmentWriter::create(dir.path().join("o.seg"), SegKind::Account, 0, 0, 10).unwrap();
        w.push(&[7u8; 32], 3, b"").unwrap();
        assert!(w.push(&[7u8; 32], 3, b"").is_err());
        assert!(w.push(&[6u8; 32], 9, b"").is_err());
    }

    #[test]
    fn strips_leading_zeros() {
        let mut v = [0u8; 32];
        assert_eq!(strip_leading_zeros(&v), &[] as &[u8]);
        v[31] = 5;
        assert_eq!(strip_leading_zeros(&v), &[5]);
        v[0] = 1;
        assert_eq!(strip_leading_zeros(&v).len(), 32);
    }
}
