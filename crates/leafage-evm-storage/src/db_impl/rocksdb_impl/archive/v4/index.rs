//! Compact in-memory page index of a v4 page file (`.cidx`, see
//! `docs/ArchiveSchemaV4.md`, "Index").
//!
//! Pages are grouped into index blocks of up to [`BLOCK_PAGES`] pages. A block
//! header keeps the full first key of its first page, the common prefix
//! length `cp` of all first keys in the block, the block's first page and
//! its file offset. Each page keeps a [`W`]-byte window of its first key
//! after the common prefix, its first block number and its stored length:
//! 14 bytes per page plus the headers, about 15 in all.
//!
//! The window of a block inside one contract (or of accounts) is the key
//! bytes after the common prefix. A block spanning contracts takes the rest
//! of the address hash's first 8 bytes, then the head of the slot hash, so
//! that it also orders the pages of one contract. A block is closed early
//! when two different consecutive first keys would share a window, or two
//! different addresses the address part of it. Within a block the windows
//! then order the pages exactly, and a lookup key is misplaced only among
//! the pages that share its window, or the address part of its window while
//! being of another address (two address hashes equal in their first 8
//! bytes, which takes about 2^32 hashes to make). The page lookup tells,
//! and the caller steps ([`Index::may_continue`]). The header search is on
//! full keys.

use super::codec::{rd32, rd64};
use super::corrupt;
use crate::db_impl::error::Error;

/// Stored pages are padded to this many bytes; offsets count these units.
pub(crate) const UNIT: u64 = 64;
/// Window bytes per page.
const W: usize = 8;
/// Pages per index block, at most.
const BLOCK_PAGES: usize = 64;

/// The file ends with the CRC32 of everything before it.
const MAGIC: &[u8; 8] = b"LFV4CI2\0";

/// High bit of an in-memory `hdr_cp` entry: this block stores 4-byte windows
/// instead of 8-byte windows (only set for `k == 32` or `cp >= 32` when
/// consecutive distinct pages keep strict ordering under `[..4]`).
const CP_NARROW: u8 = 0x80;
const CP_MASK: u8 = 0x7f;

fn lcp(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// Bytes of the window that come from the address hash, in a block of
/// storage keys with common prefix `cp`.
fn address_part(k: usize, cp: usize) -> usize {
    if k != 64 || cp >= 32 {
        0
    } else if cp < 8 {
        8 - cp
    } else {
        W.min(32 - cp)
    }
}

/// The window of a key in a block with common prefix `cp`: the key bytes
/// after the prefix, except that a block of storage keys sharing fewer than
/// 8 bytes takes the address hash up to its byte 8 and then the head of the
/// slot hash.
fn window(key: &[u8], cp: usize) -> [u8; W] {
    let mut w = [0u8; W];
    if key.len() == 64 && cp < 8 {
        let a = 8 - cp;
        w[..a].copy_from_slice(&key[cp..8]);
        w[a..].copy_from_slice(&key[32..32 + cp]);
    } else {
        let tail = &key[cp.min(key.len())..];
        let n = tail.len().min(W);
        w[..n].copy_from_slice(&tail[..n]);
    }
    w
}

/// Collects the first key, first block and offset of each page as a file is
/// written.
pub(crate) struct IndexBuilder {
    k: usize,
    keys: Vec<u8>,
    blks: Vec<u32>,
    /// Page offsets in [`UNIT`]s.
    offs: Vec<u64>,
}

impl IndexBuilder {
    pub(crate) fn new(k: usize) -> Self {
        Self {
            k,
            keys: Vec::new(),
            blks: Vec::new(),
            offs: Vec::new(),
        }
    }

    /// A page starting at byte `offset` whose first record is `(key, block)`.
    pub(crate) fn push(&mut self, key: &[u8], block: u64, offset: u64) {
        debug_assert_eq!(key.len(), self.k);
        debug_assert_eq!(offset % UNIT, 0);
        self.keys.extend_from_slice(key);
        self.blks.push(block as u32);
        self.offs.push(offset / UNIT);
    }

    /// Serialize the index of a file of `end` bytes.
    pub(crate) fn finish(mut self, end: u64) -> Result<Vec<u8>, Error> {
        let (k, n) = (self.k, self.blks.len());
        self.offs.push(end / UNIT);
        let key = |i: usize| &self.keys[i * k..(i + 1) * k];
        let mut hdr_keys = Vec::new();
        let mut hdr_cp = Vec::new();
        let mut hdr_start: Vec<u32> = Vec::new();
        let mut hdr_base: Vec<u64> = Vec::new();
        let mut win = vec![0u8; n * W];
        // A page that starts with the key its neighbour starts with: the key
        // has more versions than a page holds.
        let in_run =
            |i: usize| (i > 0 && key(i) == key(i - 1)) || (i + 1 < n && key(i) == key(i + 1));
        let mut s = 0usize;
        while s < n {
            let mut e = s + 1;
            let mut cp = k;
            let mut has_run = in_run(s);
            while e < n && e - s < BLOCK_PAGES {
                let ncp = lcp(key(s), key(e)).min(cp);
                // In a storage block spanning contracts (`ncp < 32`), a key
                // starting many pages cannot share the block with another
                // contract: its pages share the address window, and a lookup
                // of another slot of that address would step through every one
                // of them. Accounts (`k == 32`) and single-contract blocks
                // (`ncp >= 32`) have `address_part == 0`, so their windows
                // distinguish distinct keys without splitting the block.
                if k == 64 && ncp < 32 && (has_run || in_run(e)) {
                    break;
                }
                // Shortening the prefix changes every window of the block.
                let ap = address_part(k, ncp);
                let ordered = (s + 1..=e).all(|j| {
                    let (a, b) = (key(j - 1), key(j));
                    let (wa, wb) = (window(a, ncp), window(b, ncp));
                    a == b || (wb > wa && (ap == 0 || a[..32] == b[..32] || wb[..ap] > wa[..ap]))
                });
                if !ordered {
                    break;
                }
                cp = ncp;
                has_run |= in_run(e);
                e += 1;
            }
            hdr_keys.extend_from_slice(key(s));
            hdr_cp.push(cp as u8);
            hdr_start.push(s as u32);
            hdr_base.push(self.offs[s]);
            for j in s..e {
                win[j * W..(j + 1) * W].copy_from_slice(&window(key(j), cp));
            }
            s = e;
        }
        let nb = hdr_start.len();
        let mut out = Vec::with_capacity(28 + nb * (k + 13) + n * (W + 6) + 4);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(k as u32).to_le_bytes());
        out.extend_from_slice(&(n as u64).to_le_bytes());
        out.extend_from_slice(&(nb as u64).to_le_bytes());
        out.extend_from_slice(&hdr_keys);
        out.extend_from_slice(&hdr_cp);
        for x in &hdr_start {
            out.extend_from_slice(&x.to_le_bytes());
        }
        for x in &hdr_base {
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.extend_from_slice(&win);
        for x in &self.blks {
            out.extend_from_slice(&x.to_le_bytes());
        }
        for i in 0..n {
            let units = self.offs[i + 1] - self.offs[i];
            let units = u16::try_from(units)
                .map_err(|_| corrupt("page longer than the index can describe"))?;
            out.extend_from_slice(&units.to_le_bytes());
        }
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        Ok(out)
    }
}

/// Block starting offsets in [`UNIT`]s: `u32` covers files up to 256 GiB.
enum BaseOffs {
    U32(Vec<u32>),
    U64(Vec<u64>),
}

impl BaseOffs {
    #[inline]
    fn get(&self, b: usize) -> u64 {
        match self {
            Self::U32(v) => v[b] as u64,
            Self::U64(v) => v[b],
        }
    }

    fn mem_bytes(&self) -> usize {
        match self {
            Self::U32(v) => v.len() * 4,
            Self::U64(v) => v.len() * 8,
        }
    }
}

/// The index of one page file, held in memory.
pub(crate) struct Index {
    k: usize,
    /// Pages.
    n: usize,
    hdr_keys: Vec<u8>,
    /// Low 7 bits: common prefix length `cp` (`0..=k`).
    /// Bit 7 ([`CP_NARROW`]): block stores 4-byte windows instead of 8-byte.
    /// Blocks with `cp == k` store 0-byte windows.
    hdr_cp: Vec<u8>,
    hdr_start: Vec<u32>,
    hdr_base: BaseOffs,
    /// Byte offset into `win` where each block's windows start.
    hdr_win_off: Vec<u32>,
    win: Vec<u8>,
    blk: Vec<u32>,
    /// Stored page length in [`UNIT`]s; `u8::MAX` is an overflow sentinel
    /// resolved via `len_hi`.
    len: Vec<u8>,
    /// Overflow page lengths (`>= 255` units), sorted by page index.
    len_hi: Vec<(u32, u16)>,
}

impl Index {
    pub(crate) fn parse(b: &[u8]) -> Result<Self, Error> {
        let bad = || corrupt("malformed page index");
        if b.len() < 28 {
            return Err(bad());
        }
        if &b[..8] != MAGIC {
            return Err(bad());
        }
        let (body, crc) = b.split_at(b.len() - 4);
        if crc32fast::hash(body) != rd32(crc, 0) {
            return Err(corrupt("page index checksum mismatch"));
        }
        let k = rd32(body, 8) as usize;
        let n = rd64(body, 12) as usize;
        let nb = rd64(body, 20) as usize;
        if !(k == 32 || k == 64)
            || nb > n
            || n > u32::MAX as usize
            || body.len() as u128
                != 28 + nb as u128 * (k as u128 + 13) + n as u128 * (W as u128 + 6)
        {
            return Err(bad());
        }
        let mut p = 28usize;
        let mut take = |len: usize| {
            let s = &body[p..p + len];
            p += len;
            s
        };
        let hdr_keys = take(nb * k).to_vec();
        let mut hdr_cp = take(nb).to_vec();
        let hdr_start: Vec<u32> = take(nb * 4)
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let raw_base = take(nb * 8);
        let raw_win = take(n * W);
        let blk: Vec<u32> = take(n * 4)
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let raw_len = take(n * 2);

        let starts_ok = hdr_start.first().is_none_or(|s| *s == 0)
            && hdr_start.windows(2).all(|w| w[0] < w[1])
            && hdr_start.last().is_none_or(|s| (*s as usize) < n);
        if !starts_ok || hdr_cp.iter().any(|cp| *cp as usize > k) || (n > 0 && nb == 0) {
            return Err(bad());
        }

        let max_base = raw_base
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .max()
            .unwrap_or(0);
        let hdr_base = if max_base <= u32::MAX as u64 {
            BaseOffs::U32(
                raw_base
                    .chunks_exact(8)
                    .map(|c| u64::from_le_bytes(c.try_into().unwrap()) as u32)
                    .collect(),
            )
        } else {
            BaseOffs::U64(
                raw_base
                    .chunks_exact(8)
                    .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
                    .collect(),
            )
        };

        let mut hdr_win_off = Vec::with_capacity(nb);
        let mut win = Vec::with_capacity(n * 4);
        let raw_w = |j: usize| &raw_win[j * W..(j + 1) * W];
        for b in 0..nb {
            let s = hdr_start[b] as usize;
            let e = hdr_start.get(b + 1).map_or(n, |&x| x as usize);
            let cp = hdr_cp[b] as usize;
            hdr_win_off.push(u32::try_from(win.len()).map_err(|_| bad())?);
            if cp == k {
                // Every page in this block shares the full header key.
                continue;
            }
            // Narrow single-contract (`cp >= 32`) and account (`k == 32`)
            // blocks from 8 to 4 bytes when distinct pages remain strictly
            // ordered under `[..4]`. Cross-contract blocks (`k == 64 && cp < 32`)
            // keep full 8-byte windows so `address_part` stays intact.
            let narrow = (k == 32 || cp >= 32)
                && (s + 1..e).all(|j| {
                    let (wa, wb) = (raw_w(j - 1), raw_w(j));
                    wa == wb || wa[..4] < wb[..4]
                });
            if narrow {
                hdr_cp[b] |= CP_NARROW;
                for j in s..e {
                    win.extend_from_slice(&raw_w(j)[..4]);
                }
            } else {
                win.extend_from_slice(&raw_win[s * W..e * W]);
            }
        }
        win.shrink_to_fit();

        let mut len = Vec::with_capacity(n);
        let mut len_hi = Vec::new();
        for (i, c) in raw_len.chunks_exact(2).enumerate() {
            let u = u16::from_le_bytes(c.try_into().unwrap());
            if u < u8::MAX as u16 {
                len.push(u as u8);
            } else {
                len.push(u8::MAX);
                len_hi.push((i as u32, u));
            }
        }
        len_hi.shrink_to_fit();

        Ok(Self {
            k,
            n,
            hdr_keys,
            hdr_cp,
            hdr_start,
            hdr_base,
            hdr_win_off,
            win,
            blk,
            len,
            len_hi,
        })
    }

    /// Bytes this index holds in memory.
    pub(crate) fn mem_bytes(&self) -> u64 {
        (self.hdr_keys.len()
            + self.hdr_cp.len()
            + self.hdr_start.len() * 4
            + self.hdr_base.mem_bytes()
            + self.hdr_win_off.len() * 4
            + self.win.len()
            + self.blk.len() * 4
            + self.len.len()
            + self.len_hi.len() * std::mem::size_of::<(u32, u16)>()) as u64
    }

    pub(crate) fn pages(&self) -> usize {
        self.n
    }

    pub(crate) fn key_len(&self) -> usize {
        self.k
    }

    /// The first record of the file: its key and block.
    pub(crate) fn first(&self) -> Option<(&[u8], u64)> {
        (self.n > 0).then(|| (self.hkey(0), self.blk[0] as u64))
    }

    fn hkey(&self, b: usize) -> &[u8] {
        &self.hdr_keys[b * self.k..(b + 1) * self.k]
    }

    #[inline]
    fn cp(&self, b: usize) -> usize {
        (self.hdr_cp[b] & CP_MASK) as usize
    }

    #[inline]
    fn wlen(&self, b: usize) -> usize {
        let raw = self.hdr_cp[b];
        if (raw & CP_MASK) as usize == self.k {
            0
        } else if raw & CP_NARROW != 0 {
            4
        } else {
            W
        }
    }

    fn block_of(&self, c: usize) -> usize {
        self.hdr_start.partition_point(|&s| s as usize <= c) - 1
    }

    fn block_end(&self, b: usize) -> usize {
        self.hdr_start.get(b + 1).map_or(self.n, |s| *s as usize)
    }

    #[inline]
    fn w(&self, b: usize, c: usize) -> &[u8] {
        let wl = self.wlen(b);
        let off = self.hdr_win_off[b] as usize + (c - self.hdr_start[b] as usize) * wl;
        &self.win[off..off + wl]
    }

    #[inline]
    fn page_len(&self, c: usize) -> u64 {
        let l = self.len[c];
        if l < u8::MAX {
            l as u64
        } else {
            let i = self
                .len_hi
                .binary_search_by_key(&(c as u32), |&(p, _)| p)
                .expect("overflow page length recorded");
            self.len_hi[i].1 as u64
        }
    }

    /// Byte range of page `c` in the data file.
    pub(crate) fn range(&self, c: usize) -> (u64, u64) {
        let b = self.block_of(c);
        let s = self.hdr_start[b] as usize;
        let mut units = self.hdr_base.get(b);
        for i in s..c {
            units += self.page_len(i);
        }
        let len = self.page_len(c);
        (units * UNIT, (units + len) * UNIT)
    }

    /// Bytes of the data file.
    pub(crate) fn data_len(&self) -> u64 {
        if self.n == 0 {
            0
        } else {
            self.range(self.n - 1).1
        }
    }

    /// The last page whose `(first key, first block) <= (key, h)`, by the
    /// index alone: exact except that a key sharing a first key's window
    /// counts as that key, so the page may be among those with that window
    /// (or the one before them) instead; the page lookup tells, and the
    /// caller steps.
    pub(crate) fn floor(&self, key: &[u8], h: u64) -> Option<usize> {
        debug_assert_eq!(key.len(), self.k);
        let hb = h.min(u32::MAX as u64) as u32;
        let nb = self.hdr_start.len();
        let (mut lo, mut hi) = (0usize, nb);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let le = match self.hkey(mid).cmp(key) {
                std::cmp::Ordering::Less => true,
                std::cmp::Ordering::Greater => false,
                std::cmp::Ordering::Equal => self.blk[self.hdr_start[mid] as usize] <= hb,
            };
            if le {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return None;
        }
        let b = lo - 1;
        let (s, e) = (self.hdr_start[b] as usize, self.block_end(b));
        let cp = self.cp(b);
        if key[..cp] != self.hkey(b)[..cp] {
            // Past every first key of the block.
            return Some(e - 1);
        }
        let wl = self.wlen(b);
        let qw = window(key, cp);
        let qw = &qw[..wl];
        let (mut lo, mut hi) = (s, e);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if (self.w(b, mid), self.blk[mid]) <= (qw, hb) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Some(lo.max(s + 1) - 1)
    }

    /// The pages that may be the last one starting at or below `(key, h)`,
    /// first and last: [`Self::floor`] and the pages it cannot tell from it.
    /// For scans, which have no page lookup to correct the floor.
    pub(crate) fn floor_range(&self, key: &[u8], h: u64) -> Option<(usize, usize)> {
        let f = self.floor(key, h)?;
        let b = self.block_of(f);
        let (s, e) = (self.hdr_start[b] as usize, self.block_end(b));
        let cp = self.cp(b);
        if key[..cp] != self.hkey(b)[..cp] {
            return Some((f, f));
        }
        let wl = self.wlen(b);
        if wl == 0 {
            // Every page of this block starts with `hkey(b) == key`:
            // `floor` compared the exact full key and block number.
            return Some((f, f));
        }
        let qw = window(key, cp);
        let part = match address_part(self.k, cp) {
            0 => wl,
            ap => ap,
        };
        let same = |c: usize| self.w(b, c)[..part] == qw[..part];
        // The page before the first one with the key's window part...
        let mut first = f;
        while first > s && same(first) {
            first -= 1;
        }
        // ...to the last one with it.
        let mut last = f;
        while last + 1 < e && same(last + 1) {
            last += 1;
        }
        Some((first, last))
    }

    /// Whether a key greater than every record of page `c` may still be on
    /// the page after it, as far as the index can tell: that page is in the
    /// same index block and has the key's window, or the address part of
    /// it (it may then be of another address with those bytes, before the
    /// key's).
    pub(crate) fn may_continue(&self, c: usize, key: &[u8]) -> bool {
        let nc = c + 1;
        if nc >= self.n {
            return false;
        }
        let b = self.block_of(c);
        if self.block_of(nc) != b {
            return false;
        }
        let cp = self.cp(b);
        if key[..cp] != self.hkey(b)[..cp] {
            return false;
        }
        let wl = self.wlen(b);
        if wl == 0 {
            return true;
        }
        let (qw, nw) = (window(key, cp), self.w(b, nc));
        match address_part(self.k, cp) {
            0 => nw == &qw[..wl],
            ap => nw[..ap] == qw[..ap],
        }
    }

    /// The block at which `key` starts on the page after `c`, when that page
    /// opens a new index block with exactly this key (index block headers
    /// hold whole keys).
    pub(crate) fn starts_next_block(&self, c: usize, key: &[u8]) -> Option<u64> {
        let nc = c + 1;
        if nc >= self.n {
            return None;
        }
        let nb = self.block_of(nc);
        (nb != self.block_of(c) && self.hkey(nb) == key).then_some(self.blk[nc] as u64)
    }

    /// The block at which `key` continues on the page after `c`, given that
    /// `key` is the last key of page `c`: `None` if that page starts with
    /// another key. When an index window cannot tell, this answers with the
    /// next page's first block, which is at worst too early.
    pub(crate) fn continues_at(&self, c: usize, key: &[u8]) -> Option<u64> {
        let nc = c + 1;
        if nc >= self.n {
            return None;
        }
        let (b, nb) = (self.block_of(c), self.block_of(nc));
        if b != nb {
            return (self.hkey(nb) == key).then_some(self.blk[nc] as u64);
        }
        let cp = self.cp(b);
        if key[..cp] != self.hkey(b)[..cp] {
            return None;
        }
        let wl = self.wlen(b);
        (wl == 0 || self.w(b, nc) == &window(key, cp)[..wl]).then_some(self.blk[nc] as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(keys: &[(Vec<u8>, u32)]) -> Index {
        let mut b = IndexBuilder::new(keys[0].0.len());
        for (i, (key, blk)) in keys.iter().enumerate() {
            b.push(key, *blk as u64, i as u64 * 3 * UNIT);
        }
        Index::parse(&b.finish(keys.len() as u64 * 3 * UNIT).unwrap()).unwrap()
    }

    fn key64(a: u64, slot: u64) -> Vec<u8> {
        let mut k = vec![0u8; 64];
        k[..8].copy_from_slice(&a.to_be_bytes());
        k[8..16].copy_from_slice(&a.wrapping_mul(31).to_be_bytes());
        k[32..40].copy_from_slice(&slot.to_be_bytes());
        k[56..64].copy_from_slice(&slot.wrapping_mul(7).to_be_bytes());
        k
    }

    /// Reference floor on the full first keys.
    fn floor_ref(keys: &[(Vec<u8>, u32)], key: &[u8], h: u64) -> Option<usize> {
        keys.iter()
            .rposition(|(k, b)| (k.as_slice(), *b as u64) <= (key, h))
    }

    /// A key that starts many pages is not put in an index block with the
    /// pages of other keys: a lookup of another slot of its address goes to
    /// the page that has it, not to one of those and from there back or
    /// forth through them all.
    #[test]
    fn a_key_of_many_pages_does_not_hide_its_neighbours() {
        // Addresses that differ in their first bytes: without the rule all
        // of this is one block, with windows of address bytes only.
        let hot = key64(3 << 32, 9 << 40);
        let mut keys: Vec<(Vec<u8>, u32)> =
            vec![(key64(1 << 32, 1 << 40), 10), (key64(2 << 32, 1 << 40), 10)];
        for b in 0..30u32 {
            keys.push((hot.clone(), 100 + b * 100));
        }
        keys.push((key64(4 << 32, 1 << 40), 10));
        let idx = build(&keys);
        // A slot of the hot key's address below it is on the page before
        // its pages; one above it, on the last of them.
        let below = key64(3 << 32, 2 << 40);
        let above = key64(3 << 32, 11 << 40);
        for h in [50u64, 150, 1500, 1_000_000] {
            assert_eq!(idx.floor(&below, h), Some(1), "below, at {h}");
            assert_eq!(idx.floor(&above, h), Some(31), "above, at {h}");
            assert!(!idx.may_continue(31, &above));
            assert_eq!(
                idx.floor(&hot, h),
                floor_ref(&keys, &hot, h),
                "the key, at {h}"
            );
        }
    }

    #[test]
    fn floor_matches_full_keys() {
        // Pages inside a few big addresses, single-page addresses, a key
        // spanning pages, and addresses equal in their first 8 bytes.
        let mut keys: Vec<(Vec<u8>, u32)> = Vec::new();
        for a in 1..40u64 {
            let slots = if a % 5 == 0 { 300 } else { 1 + a % 3 };
            for s in 0..slots {
                keys.push((key64(a << 32, s << 40), 10));
            }
            if a % 7 == 0 {
                let k = key64(a << 32, 5 << 40);
                for b in [100u32, 2000, 30000] {
                    keys.push((k.clone(), b));
                }
            }
        }
        let mut twin = key64(41 << 32, 1);
        keys.push((twin.clone(), 5));
        twin[20] = 9; // same first 8 address bytes, different address
        keys.push((twin, 5));
        keys.sort();
        keys.dedup();
        let idx = build(&keys);
        assert_eq!(idx.pages(), keys.len());
        for (i, _) in keys.iter().enumerate() {
            assert_eq!(
                idx.range(i),
                (i as u64 * 3 * UNIT, (i as u64 + 1) * 3 * UNIT)
            );
        }
        assert_eq!(idx.data_len(), keys.len() as u64 * 3 * UNIT);
        // The twins share an index window with each other and with the
        // keys made from them below.
        let twins = |key: &[u8]| key[..8] == (41u64 << 32).to_be_bytes();
        for (key, blk) in &keys {
            for h in [0, *blk as u64 - 1, *blk as u64, *blk as u64 + 1, u64::MAX] {
                let want = floor_ref(&keys, key, h);
                let got = idx.floor(key, h);
                if !twins(key) {
                    assert_eq!(got, want, "a first key at {h}");
                }
                let range = idx.floor_range(key, h);
                assert_eq!(range.is_some(), want.is_some());
                if let (Some((a, b)), Some(w)) = (range, want) {
                    assert!(a <= w && w <= b && a <= got.unwrap() && got.unwrap() <= b);
                }
            }
            // Keys just around a first key, and in its address with other
            // slots: the floor may be off among the pages sharing the
            // window, never beyond them.
            for (at, d) in [
                (63, 1u8),
                (63, 0xff),
                (40, 1),
                (33, 1),
                (33, 0xff),
                (20, 1),
                (31, 1),
            ] {
                let mut near = key.clone();
                near[at] = near[at].wrapping_add(d);
                for h in [0, 50, u64::MAX] {
                    let want = floor_ref(&keys, &near, h);
                    let range = idx.floor_range(&near, h);
                    match (range, want) {
                        (Some((a, b)), Some(w)) => {
                            assert!(a <= w && w <= b, "{w} outside {a}..={b}");
                            assert!(b - a < BLOCK_PAGES);
                        }
                        // Below the first page by full key, or by window.
                        (None, None) | (Some((0, _)), None) => {}
                        other => panic!("{other:?}"),
                    }
                }
            }
        }
        assert_eq!(idx.floor(&[0u8; 64], 9), None);
        assert_eq!(idx.floor(&[0xffu8; 64], 9), Some(keys.len() - 1));
    }

    #[test]
    fn continuation() {
        let k1 = key64(1 << 32, 1 << 40);
        let k2 = key64(1 << 32, 2 << 40);
        let keys = vec![
            (k1.clone(), 10),
            (k1.clone(), 500),
            (k2.clone(), 7),
            (key64(9 << 32, 0), 3),
        ];
        let idx = build(&keys);
        assert_eq!(idx.continues_at(0, &k1), Some(500));
        assert_eq!(idx.continues_at(1, &k1), None);
        assert_eq!(idx.continues_at(2, &k2), None);
        assert_eq!(idx.continues_at(3, &keys[3].0), None);
        assert!(idx.may_continue(0, &k1));
        // The pages of `k1` are an index block of their own: what follows
        // them starts with another key, and nothing of `k1`'s is there.
        assert!(!idx.may_continue(1, &k1));
        assert!(!idx.may_continue(2, &k2));
        // In a block spanning contracts, the next page of the same address
        // may be of another address with the same leading bytes.
        let spanning = vec![
            (key64(1 << 32, 1 << 40), 10),
            (key64(1 << 32, 2 << 40), 10),
            (key64(9 << 32, 0), 3),
        ];
        let idx = build(&spanning);
        assert!(idx.may_continue(0, &spanning[0].0));
        // Inside one contract, only an equal window continues.
        let one: Vec<(Vec<u8>, u32)> = (1..6u64).map(|s| (key64(1 << 32, s << 40), 3)).collect();
        let idx = build(&one);
        assert!(!idx.may_continue(1, &one[1].0));
        assert_eq!(idx.floor_range(&one[2].0, 9), Some((1, 2)));
    }

    #[test]
    fn rejects_damage() {
        let keys: Vec<(Vec<u8>, u32)> = (1..200u64).map(|a| (key64(a << 40, a), 1)).collect();
        let mut b = IndexBuilder::new(64);
        for (i, (key, blk)) in keys.iter().enumerate() {
            b.push(key, *blk as u64, i as u64 * UNIT);
        }
        let bytes = b.finish(keys.len() as u64 * UNIT).unwrap();
        assert!(Index::parse(&bytes).is_ok());
        let mut flipped = bytes.clone();
        flipped[bytes.len() / 2] ^= 1;
        assert!(Index::parse(&flipped).is_err());
        assert!(Index::parse(&bytes[..bytes.len() - 9]).is_err());
    }

    fn key32(a: u64) -> Vec<u8> {
        let mut k = vec![0u8; 32];
        k[..8].copy_from_slice(&a.to_be_bytes());
        k[24..32].copy_from_slice(&a.wrapping_mul(17).to_be_bytes());
        k
    }

    #[test]
    fn accounts_and_single_contract_runs_do_not_split_blocks() {
        // Accounts (k = 32) with interleaved multi-page keys pack into a single
        // block of up to BLOCK_PAGES pages with 4-byte windows.
        let mut acct_keys: Vec<(Vec<u8>, u32)> = Vec::new();
        for a in 1..20u64 {
            let pages = if a % 3 == 0 { 4 } else { 1 };
            for p in 0..pages {
                acct_keys.push((key32(a << 32), 10 + p * 100));
            }
        }
        assert!(acct_keys.len() <= BLOCK_PAGES);
        let idx = build(&acct_keys);
        assert_eq!(idx.hdr_start.len(), 1, "accounts should not split on in_run");
        assert_eq!(idx.wlen(0), 4);
        for (key, blk) in &acct_keys {
            for h in [0, *blk as u64 - 1, *blk as u64, *blk as u64 + 1, u64::MAX] {
                assert_eq!(idx.floor(key, h), floor_ref(&acct_keys, key, h));
            }
        }

        // Single-contract storage (cp >= 32) with interleaved multi-page slots
        // also packs into a single block with 4-byte windows.
        let mut slot_keys: Vec<(Vec<u8>, u32)> = Vec::new();
        for s in 1..20u64 {
            let pages = if s % 4 == 0 { 5 } else { 1 };
            for p in 0..pages {
                slot_keys.push((key64(7 << 32, s << 40), 20 + p * 50));
            }
        }
        assert!(slot_keys.len() <= BLOCK_PAGES);
        let idx = build(&slot_keys);
        assert_eq!(
            idx.hdr_start.len(),
            1,
            "single-contract storage should not split on in_run"
        );
        assert_eq!(idx.wlen(0), 4);
        for (key, blk) in &slot_keys {
            for h in [0, *blk as u64 - 1, *blk as u64, *blk as u64 + 1, u64::MAX] {
                assert_eq!(idx.floor(key, h), floor_ref(&slot_keys, key, h));
            }
        }
    }

    #[test]
    fn compact_index_handles_len_overflow_and_window_widths() {
        // 1) Page lengths >= 255 units (including exact 255 sentinel).
        let mut b = IndexBuilder::new(32);
        let lens = [1u64, 254, 255, 256, 1024, 3];
        let mut off = 0u64;
        for (i, &u) in lens.iter().enumerate() {
            b.push(&key32((i as u64 + 1) << 32), 10, off);
            off += u * UNIT;
        }
        let idx = Index::parse(&b.finish(off).unwrap()).unwrap();
        assert_eq!(idx.len_hi.len(), 3);
        let mut expected = 0u64;
        for (i, &u) in lens.iter().enumerate() {
            assert_eq!(idx.range(i), (expected, expected + u * UNIT));
            expected += u * UNIT;
        }
        assert_eq!(idx.data_len(), off);

        // 2) Pure run block (`cp == k`) uses 0-byte windows (`wlen == 0`) and
        //    distinguishes `key == hkey(b)` from `key != hkey(b)` in `continues_at`.
        let hot = key64(5 << 32, 9 << 40);
        let run: Vec<(Vec<u8>, u32)> = (0..10u32).map(|i| (hot.clone(), 100 + i * 50)).collect();
        let idx = build(&run);
        assert_eq!(idx.hdr_start.len(), 1);
        assert_eq!(idx.wlen(0), 0);
        assert!(idx.win.is_empty());
        assert_eq!(idx.continues_at(0, &hot), Some(150));
        assert_eq!(idx.continues_at(0, &key64(5 << 32, 10 << 40)), None);
        assert!(idx.may_continue(0, &hot));
        assert!(!idx.may_continue(0, &key64(5 << 32, 10 << 40)));
        assert_eq!(idx.floor(&hot, 220), Some(2));
        assert_eq!(idx.floor_range(&hot, 220), Some((2, 2)));

        // 3) Two distinct keys inside one contract sharing the first 4 bytes of
        //    their window keep `wlen == 8` so ordering stays exact.
        let mut s1 = vec![0u8; 64];
        s1[0] = 1;
        s1[32..36].copy_from_slice(&[0x10, 0x20, 0x30, 0x40]);
        s1[36..40].copy_from_slice(&[0, 0, 0, 1]);
        let mut s2 = s1.clone();
        s2[36..40].copy_from_slice(&[0, 0, 0, 2]);
        let mut s3 = s1.clone();
        s3[32] = 0x11; // ensures `cp == 32`, so window starts at byte 32
        let keys = vec![(s1.clone(), 10), (s2.clone(), 10), (s3.clone(), 10)];
        let idx = build(&keys);
        assert_eq!(idx.hdr_start.len(), 1);
        assert_eq!(idx.wlen(0), 8);
        for (k, blk) in &keys {
            assert_eq!(idx.floor(k, *blk as u64), floor_ref(&keys, k, *blk as u64));
        }

        // 4) Memory accounting reflects the narrower in-memory encoding.
        let big: Vec<(Vec<u8>, u32)> = (1..=512u64)
            .map(|s| (key64(1 << 32, s << 40), 10))
            .collect();
        let idx = build(&big);
        // Uncompressed would be >= 512 * 14 = 7168 B; compact is 9 B/page + headers (~5200 B).
        assert!(
            idx.mem_bytes() < 512 * 11,
            "expected compact index < 5632 B, got {}",
            idx.mem_bytes()
        );
    }
}
