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
            while e < n && e - s < BLOCK_PAGES {
                // The pages of such a key get index blocks of their own.
                // Among the pages of other keys they would all have the
                // window those of their address have, and a lookup of
                // another slot of it would step through every one of them.
                if in_run(e) != in_run(s) || (in_run(s) && key(e) != key(s)) {
                    break;
                }
                let ncp = lcp(key(s), key(e)).min(cp);
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

/// The index of one page file, held in memory.
pub(crate) struct Index {
    k: usize,
    /// Pages.
    n: usize,
    hdr_keys: Vec<u8>,
    hdr_cp: Vec<u8>,
    hdr_start: Vec<u32>,
    hdr_base: Vec<u64>,
    win: Vec<u8>,
    blk: Vec<u32>,
    len: Vec<u16>,
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
        let hdr_cp = take(nb).to_vec();
        let hdr_start: Vec<u32> = take(nb * 4)
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let hdr_base: Vec<u64> = take(nb * 8)
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let win = take(n * W).to_vec();
        let blk: Vec<u32> = take(n * 4)
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let len: Vec<u16> = take(n * 2)
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let starts_ok = hdr_start.first().is_none_or(|s| *s == 0)
            && hdr_start.windows(2).all(|w| w[0] < w[1])
            && hdr_start.last().is_none_or(|s| (*s as usize) < n);
        if !starts_ok || hdr_cp.iter().any(|cp| *cp as usize > k) || (n > 0 && nb == 0) {
            return Err(bad());
        }
        Ok(Self {
            k,
            n,
            hdr_keys,
            hdr_cp,
            hdr_start,
            hdr_base,
            win,
            blk,
            len,
        })
    }

    /// Bytes this index holds in memory.
    pub(crate) fn mem_bytes(&self) -> u64 {
        (self.hdr_keys.len()
            + self.hdr_cp.len()
            + self.hdr_start.len() * 4
            + self.hdr_base.len() * 8
            + self.win.len()
            + self.blk.len() * 4
            + self.len.len() * 2) as u64
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

    fn block_of(&self, c: usize) -> usize {
        self.hdr_start.partition_point(|&s| s as usize <= c) - 1
    }

    fn block_end(&self, b: usize) -> usize {
        self.hdr_start.get(b + 1).map_or(self.n, |s| *s as usize)
    }

    fn w(&self, c: usize) -> &[u8] {
        &self.win[c * W..(c + 1) * W]
    }

    /// Byte range of page `c` in the data file.
    pub(crate) fn range(&self, c: usize) -> (u64, u64) {
        let b = self.block_of(c);
        let s = self.hdr_start[b] as usize;
        let units: u64 = self.hdr_base[b] + self.len[s..c].iter().map(|&x| x as u64).sum::<u64>();
        (units * UNIT, (units + self.len[c] as u64) * UNIT)
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
        let cp = self.hdr_cp[b] as usize;
        if key[..cp] != self.hkey(b)[..cp] {
            // Past every first key of the block.
            return Some(e - 1);
        }
        let qw = window(key, cp);
        let (mut lo, mut hi) = (s, e);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if (self.w(mid), self.blk[mid]) <= (&qw[..], hb) {
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
        let cp = self.hdr_cp[b] as usize;
        if key[..cp] != self.hkey(b)[..cp] {
            return Some((f, f));
        }
        let qw = window(key, cp);
        let part = match address_part(self.k, cp) {
            0 => W,
            ap => ap,
        };
        let same = |c: usize| self.w(c)[..part] == qw[..part];
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
        let cp = self.hdr_cp[b] as usize;
        if key[..cp] != self.hkey(b)[..cp] {
            return false;
        }
        let (qw, nw) = (window(key, cp), self.w(nc));
        match address_part(self.k, cp) {
            0 => nw == &qw[..],
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
        let cp = self.hdr_cp[b] as usize;
        (self.w(nc) == &window(key, cp)[..]).then_some(self.blk[nc] as u64)
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
}
