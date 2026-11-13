//! Page codec of archive schema v4 (`docs/ArchiveSchemaV4.md`, "Page").
//!
//! A page holds versions `(key, block, value)` sorted by key then block, as
//! eight byte streams: a key is stored once, a block as a delta to the key's
//! previous version, a value as the shortest of raw / xor / difference against
//! the key's previous value. Every `R`-th record is a restart: the value
//! state resets there and a table entry records the stream offsets, so a
//! lookup binary-searches the restarts and decodes at most `R` records.

use super::corrupt;
use crate::db_impl::error::Error;

/// `keccak256("")`, the code hash of an account without code; the encoder's
/// initial code hash, so such accounts store none.
pub(crate) const KECCAK_EMPTY: [u8; 32] = [
    0xc5, 0xd2, 0x46, 0x01, 0x86, 0xf7, 0x23, 0x3c, 0x92, 0x7e, 0x7d, 0xb2, 0xdc, 0xc7, 0x03, 0xc0,
    0xe5, 0x00, 0xb6, 0x53, 0xca, 0x82, 0x27, 0x3b, 0x7b, 0xfa, 0xd8, 0x04, 0x5d, 0x85, 0xa4, 0x70,
];

/// Raw size at which a page is closed. Measured on bsc: smaller pages lose
/// the prefetch of neighbouring keys, larger ones cost cache memory.
pub(crate) const PAGE_TARGET: usize = 12 << 10;

/// Restart period in records.
const R_STORAGE: usize = 32;
const R_ACCOUNT: usize = 64;
/// Restart entry sizes.
///   storage: block u32 | addr_off u16 | key_off u16 | blk_off u16 | val_off u16
///   account: block u32 | key_off u16 | blk_off u16 | nonce_off u16 | val_off u16 | code_cur u16 | code_off u16
const ES_STORAGE: usize = 12;
const ES_ACCOUNT: usize = 16;
const NO_CODE: u16 = u16::MAX;

/// Account tag bits.
const T_DELETED: u8 = 1;
const T_NONCE: u8 = 2;
const T_BALANCE: u8 = 4;
const T_CODE: u8 = 8;
/// The nonce is stored whole (it went down), not as a difference.
const T_NONCE_ABS: u8 = 16;
/// First record of a key.
const T_NEW: u8 = 32;

/// Storage kinds: 0 same key, 1 new slot of the same address, 2 new address.
const K_SAME: u8 = 0;
const K_SLOT: u8 = 1;
const K_ADDR: u8 = 2;

/// Key length of a kind: an account key is the address hash, a storage key
/// the address hash then the slot hash.
#[inline]
pub(crate) const fn key_len(storage: bool) -> usize {
    if storage {
        64
    } else {
        32
    }
}

/// A version's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Val {
    /// A slot's value, big-endian.
    Storage([u8; 32]),
    /// The account was deleted at this block.
    Deleted,
    Account {
        nonce: u64,
        /// Big-endian.
        balance: [u8; 32],
        code_hash: [u8; 32],
    },
}

#[inline]
pub(crate) fn rd16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
#[inline]
pub(crate) fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
#[inline]
pub(crate) fn rd64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[inline]
pub(crate) fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline]
pub(crate) fn get_varint(b: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let c = b[*pos];
        *pos += 1;
        v |= ((c & 0x7f) as u64) << shift;
        if c < 0x80 || shift >= 63 {
            return v;
        }
        shift += 7;
    }
}

#[inline]
fn skip_varint(b: &[u8], pos: &mut usize) {
    while b[*pos] >= 0x80 {
        *pos += 1;
    }
    *pos += 1;
}

#[inline]
fn pad32(v: &[u8]) -> [u8; 32] {
    let mut o = [0u8; 32];
    o[32 - v.len()..].copy_from_slice(v);
    o
}

#[inline]
pub(crate) fn strip(v: &[u8]) -> &[u8] {
    let z = v.iter().take_while(|b| **b == 0).count();
    &v[z..]
}

fn sub256(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut o = [0u8; 32];
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let mut d = a[i] as i16 - b[i] as i16 - borrow;
        if d < 0 {
            d += 256;
            borrow = 1;
        } else {
            borrow = 0;
        }
        o[i] = d as u8;
    }
    o
}

fn add256(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut o = [0u8; 32];
    let mut carry = 0u16;
    for i in (0..32).rev() {
        let s = a[i] as u16 + b[i] as u16 + carry;
        o[i] = s as u8;
        carry = s >> 8;
    }
    o
}

/// Encode `cur` against `prev` as the shortest of raw, xor, `cur - prev`
/// and `prev - cur`, appending the bytes to `out`. Returns the tag: low 6
/// bits the length, high 2 the mode.
fn delta_encode(prev: &[u8; 32], cur: &[u8; 32], out: &mut Vec<u8>) -> u8 {
    let raw_len = strip(cur).len();
    let mut x = [0u8; 32];
    for i in 0..32 {
        x[i] = cur[i] ^ prev[i];
    }
    let xor_len = strip(&x).len();
    let (diff_mode, d) = if cur >= prev {
        (2u8, sub256(cur, prev))
    } else {
        (3u8, sub256(prev, cur))
    };
    let diff_len = strip(&d).len();
    let (mode, bytes): (u8, &[u8; 32]) = if diff_len < raw_len.min(xor_len) {
        (diff_mode, &d)
    } else if xor_len < raw_len {
        (1, &x)
    } else {
        (0, cur)
    };
    let s = strip(bytes);
    out.extend_from_slice(s);
    (mode << 6) | s.len() as u8
}

#[inline]
fn delta_decode(prev: &[u8; 32], tag: u8, bytes: &[u8]) -> [u8; 32] {
    let d = pad32(bytes);
    match tag >> 6 {
        0 => d,
        1 => {
            let mut o = [0u8; 32];
            for i in 0..32 {
                o[i] = prev[i] ^ d[i];
            }
            o
        }
        2 => add256(prev, &d),
        _ => sub256(prev, &d),
    }
}

/// Builds one page at a time.
pub(crate) struct PageEnc {
    storage: bool,
    r: usize,
    kinds: Vec<u8>,
    keys: Vec<u8>,
    blocks: Vec<u8>,
    tags: Vec<u8>,
    nonce: Vec<u8>,
    vals: Vec<u8>,
    codes: Vec<u8>,
    rst: Vec<u8>,
    n: usize,
    cur: [u8; 64],
    have: bool,
    addr_off: u16,
    key_off: u16,
    last_block: u64,
    p_val: [u8; 32],
    p_nonce: u64,
    p_code: [u8; 32],
    code_off: u16,
}

impl PageEnc {
    pub(crate) fn new(storage: bool) -> Self {
        Self {
            storage,
            r: if storage { R_STORAGE } else { R_ACCOUNT },
            kinds: Vec::new(),
            keys: Vec::new(),
            blocks: Vec::new(),
            tags: Vec::new(),
            nonce: Vec::new(),
            vals: Vec::new(),
            codes: Vec::new(),
            rst: Vec::new(),
            n: 0,
            cur: [0; 64],
            have: false,
            addr_off: 0,
            key_off: 0,
            last_block: 0,
            p_val: [0; 32],
            p_nonce: 0,
            p_code: KECCAK_EMPTY,
            code_off: NO_CODE,
        }
    }

    /// Records in the open page.
    pub(crate) fn len(&self) -> usize {
        self.n
    }

    /// Raw size of the open page.
    pub(crate) fn size(&self) -> usize {
        self.kinds.len()
            + self.keys.len()
            + self.blocks.len()
            + self.tags.len()
            + self.nonce.len()
            + self.vals.len()
            + self.codes.len()
            + self.rst.len()
    }

    /// Append a version. Versions come in (key, block) order, and a page is
    /// closed ([`Self::finish`]) once it reaches [`PAGE_TARGET`], which keeps
    /// every stream offset within the restart table's `u16`.
    pub(crate) fn add(&mut self, key: &[u8], block: u64, val: &Val) {
        let k = key.len();
        debug_assert_eq!(k, key_len(self.storage));
        debug_assert!(block <= u32::MAX as u64);
        let new_key = !self.have || self.cur[..k] != *key;
        debug_assert!(!self.have || (&self.cur[..k], self.last_block) < (key, block));
        if new_key {
            if self.storage {
                if self.have && self.cur[..32] == key[..32] {
                    self.kinds.push(K_SLOT);
                } else {
                    self.kinds.push(K_ADDR);
                    self.addr_off = self.keys.len() as u16;
                    self.keys.extend_from_slice(&key[..32]);
                }
                self.key_off = self.keys.len() as u16;
                self.keys.extend_from_slice(&key[32..64]);
            } else {
                self.key_off = self.keys.len() as u16;
                self.keys.extend_from_slice(key);
            }
            self.cur[..k].copy_from_slice(key);
            self.have = true;
            self.p_val = [0; 32];
            self.p_nonce = 0;
            self.p_code = KECCAK_EMPTY;
            self.code_off = NO_CODE;
        } else if self.storage {
            self.kinds.push(K_SAME);
        }
        if self.n % self.r == 0 {
            self.p_val = [0; 32];
            self.p_nonce = 0;
            self.rst.extend_from_slice(&(block as u32).to_le_bytes());
            let offs: [usize; 6] = if self.storage {
                [
                    self.addr_off as usize,
                    self.key_off as usize,
                    self.blocks.len(),
                    self.vals.len(),
                    0,
                    0,
                ]
            } else {
                [
                    self.key_off as usize,
                    self.blocks.len(),
                    self.nonce.len(),
                    self.vals.len(),
                    self.codes.len(),
                    self.code_off as usize,
                ]
            };
            let used = if self.storage { 4 } else { 6 };
            for &o in &offs[..used] {
                assert!(o <= u16::MAX as usize, "v4 page stream over 64 KiB");
                self.rst.extend_from_slice(&(o as u16).to_le_bytes());
            }
        }
        put_varint(
            &mut self.blocks,
            if new_key {
                block
            } else {
                block - self.last_block
            },
        );
        self.last_block = block;
        self.n += 1;
        let nk = if new_key { T_NEW } else { 0 };
        match val {
            Val::Storage(v) => {
                debug_assert!(self.storage);
                let tag = delta_encode(&self.p_val, v, &mut self.vals);
                self.p_val = *v;
                self.tags.push(tag);
            }
            Val::Deleted => {
                debug_assert!(!self.storage);
                self.tags.push(T_DELETED | nk);
                self.p_nonce = 0;
                self.p_val = [0; 32];
                self.p_code = KECCAK_EMPTY;
                self.code_off = NO_CODE;
            }
            Val::Account {
                nonce,
                balance,
                code_hash,
            } => {
                debug_assert!(!self.storage);
                let mut tag = nk;
                if *nonce != self.p_nonce {
                    tag |= T_NONCE;
                    if *nonce >= self.p_nonce {
                        put_varint(&mut self.nonce, nonce - self.p_nonce);
                    } else {
                        tag |= T_NONCE_ABS;
                        put_varint(&mut self.nonce, *nonce);
                    }
                }
                if *balance != self.p_val {
                    tag |= T_BALANCE;
                    let at = self.vals.len();
                    self.vals.push(0);
                    let t = delta_encode(&self.p_val, balance, &mut self.vals);
                    self.vals[at] = t;
                }
                if *code_hash != self.p_code {
                    tag |= T_CODE;
                    self.code_off = self.codes.len() as u16;
                    self.codes.extend_from_slice(code_hash);
                }
                self.tags.push(tag);
                self.p_nonce = *nonce;
                self.p_val = *balance;
                self.p_code = *code_hash;
            }
        }
    }

    /// Close the page into `out` (cleared first) and start a new one.
    pub(crate) fn finish(&mut self, out: &mut Vec<u8>) {
        out.clear();
        out.reserve(self.size() + 32);
        put_varint(out, self.n as u64);
        for s in [
            &mut self.kinds,
            &mut self.keys,
            &mut self.blocks,
            &mut self.tags,
            &mut self.nonce,
            &mut self.vals,
            &mut self.codes,
            &mut self.rst,
        ] {
            put_varint(out, s.len() as u64);
            out.extend_from_slice(s);
            s.clear();
        }
        self.n = 0;
        self.have = false;
    }
}

/// The streams of a raw page.
struct Streams<'a> {
    n: usize,
    kinds: &'a [u8],
    keys: &'a [u8],
    blocks: &'a [u8],
    tags: &'a [u8],
    nonce: &'a [u8],
    vals: &'a [u8],
    codes: &'a [u8],
    rst: &'a [u8],
}

impl<'a> Streams<'a> {
    /// Split a page and check that the streams are consistent with the
    /// record count, so that decoding a page that passed its checksum
    /// cannot index out of bounds on the fixed-size streams.
    fn parse(page: &'a [u8], storage: bool) -> Result<Self, Error> {
        let bad = || corrupt("malformed page");
        let mut pos = 0usize;
        let varint = |pos: &mut usize| -> Result<usize, Error> {
            let mut v = 0u64;
            let mut shift = 0;
            loop {
                let c = *page.get(*pos).ok_or_else(bad)?;
                *pos += 1;
                v |= ((c & 0x7f) as u64) << shift;
                if c < 0x80 {
                    return Ok(v as usize);
                }
                shift += 7;
                if shift > 28 {
                    return Err(bad());
                }
            }
        };
        let n = varint(&mut pos)?;
        let mut s: [&[u8]; 8] = [&[]; 8];
        for x in s.iter_mut() {
            let len = varint(&mut pos)?;
            *x = page.get(pos..pos + len).ok_or_else(bad)?;
            pos += len;
        }
        let [kinds, keys, blocks, tags, nonce, vals, codes, rst] = s;
        let (r, es) = if storage {
            (R_STORAGE, ES_STORAGE)
        } else {
            (R_ACCOUNT, ES_ACCOUNT)
        };
        if n == 0
            || tags.len() != n
            || (storage && kinds.len() != n)
            || rst.len() != n.div_ceil(r) * es
        {
            return Err(bad());
        }
        Ok(Self {
            n,
            kinds,
            keys,
            blocks,
            tags,
            nonce,
            vals,
            codes,
            rst,
        })
    }
}

/// Result of [`lookup`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Look {
    /// The greatest version `<= h` of the key: its block and value.
    Found(u64, Val),
    /// The key's place is in this page and it has no version `<= h`.
    Absent,
    /// Every record of the page is greater than `(key, h)`.
    BeforeFirst,
    /// Every record of the page is less than `(key, h)` and none is the key.
    AfterLast,
}

#[inline]
fn key_cmp(
    storage: bool,
    keys: &[u8],
    addr_off: usize,
    key_off: usize,
    key: &[u8],
) -> std::cmp::Ordering {
    if storage {
        keys[addr_off..addr_off + 32]
            .cmp(&key[..32])
            .then_with(|| keys[key_off..key_off + 32].cmp(&key[32..64]))
    } else {
        keys[key_off..key_off + 32].cmp(&key[..32])
    }
}

/// What a page knows about the key's next version after a [`lookup`]
/// answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// The key's next version is at this block.
    At(u64),
    /// The key has no later version in this file.
    None,
    /// The page ended at the answer; the next page starts the continuation.
    PageEnd,
}

/// The greatest version `<= h` of `key` in a raw page, and what follows it.
/// The second value is only meaningful with [`Look::Found`] and
/// [`Look::Absent`].
pub(crate) fn lookup(
    page: &[u8],
    storage: bool,
    key: &[u8],
    h: u64,
) -> Result<(Look, Next), Error> {
    use std::cmp::Ordering::*;
    let Streams {
        n,
        kinds,
        keys,
        blocks,
        tags,
        nonce,
        vals,
        codes,
        rst,
    } = Streams::parse(page, storage)?;
    let (r, es) = if storage {
        (R_STORAGE, ES_STORAGE)
    } else {
        (R_ACCOUNT, ES_ACCOUNT)
    };
    let nr = rst.len() / es;
    let rkey = |i: usize| -> (usize, usize) {
        let e = i * es;
        if storage {
            (rd16(rst, e + 4) as usize, rd16(rst, e + 6) as usize)
        } else {
            (0, rd16(rst, e + 4) as usize)
        }
    };
    // Last restart with (key_i, block_i) <= (key, h).
    let (mut lo, mut hi) = (0usize, nr);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (a, k) = rkey(mid);
        let le = match key_cmp(storage, keys, a, k, key) {
            Less => true,
            Greater => false,
            Equal => rd32(rst, mid * es) as u64 <= h,
        };
        if le {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == 0 {
        return Ok((Look::BeforeFirst, Next::PageEnd));
    }
    let i = lo - 1;
    let e = i * es;
    let mut block = rd32(rst, e) as u64;
    let (mut addr_off, mut key_off) = rkey(i);
    let (mut bi, mut vi, mut ni, mut di);
    let mut p_code = KECCAK_EMPTY;
    if storage {
        bi = rd16(rst, e + 8) as usize;
        vi = rd16(rst, e + 10) as usize;
        ni = 0;
        di = 0;
    } else {
        bi = rd16(rst, e + 6) as usize;
        ni = rd16(rst, e + 8) as usize;
        vi = rd16(rst, e + 10) as usize;
        di = rd16(rst, e + 12) as usize;
        let co = rd16(rst, e + 14);
        if co != NO_CODE {
            p_code.copy_from_slice(&codes[co as usize..co as usize + 32]);
        }
    }
    let mut kc = key_off + 32;
    let mut p_val = [0u8; 32];
    let mut p_nonce = 0u64;
    let mut best: Option<(u64, Val)> = None;
    let mut j = i * r;
    while j < n {
        let tag = tags[j];
        if j == i * r {
            skip_varint(blocks, &mut bi);
        } else {
            let new_key = if storage {
                kinds[j] != K_SAME
            } else {
                tag & T_NEW != 0
            };
            if new_key {
                if storage && kinds[j] == K_ADDR {
                    addr_off = kc;
                    kc += 32;
                }
                key_off = kc;
                kc += 32;
                block = get_varint(blocks, &mut bi);
                p_val = [0; 32];
                p_nonce = 0;
                p_code = KECCAK_EMPTY;
            } else {
                block += get_varint(blocks, &mut bi);
            }
            if j % r == 0 {
                p_val = [0; 32];
                p_nonce = 0;
            }
        }
        let c = key_cmp(storage, keys, addr_off, key_off, key);
        if c == Greater || (c == Equal && block > h) {
            let next = if c == Equal {
                Next::At(block)
            } else {
                Next::None
            };
            return Ok(match best {
                Some((b, v)) => (Look::Found(b, v), next),
                None => (Look::Absent, next),
            });
        }
        if c == Less {
            // Skip the value.
            if storage {
                vi += (tag & 63) as usize;
            } else if tag & T_DELETED == 0 {
                if tag & T_NONCE != 0 {
                    skip_varint(nonce, &mut ni);
                }
                if tag & T_BALANCE != 0 {
                    vi += 1 + (vals[vi] & 63) as usize;
                }
                if tag & T_CODE != 0 {
                    di += 32;
                }
            }
            best = None;
        } else if storage {
            let len = (tag & 63) as usize;
            p_val = delta_decode(&p_val, tag, &vals[vi..vi + len]);
            vi += len;
            best = Some((block, Val::Storage(p_val)));
        } else if tag & T_DELETED != 0 {
            p_nonce = 0;
            p_val = [0; 32];
            p_code = KECCAK_EMPTY;
            best = Some((block, Val::Deleted));
        } else {
            if tag & T_NONCE != 0 {
                let x = get_varint(nonce, &mut ni);
                p_nonce = if tag & T_NONCE_ABS != 0 {
                    x
                } else {
                    p_nonce + x
                };
            }
            if tag & T_BALANCE != 0 {
                let t = vals[vi];
                let len = (t & 63) as usize;
                p_val = delta_decode(&p_val, t, &vals[vi + 1..vi + 1 + len]);
                vi += 1 + len;
            }
            if tag & T_CODE != 0 {
                p_code.copy_from_slice(&codes[di..di + 32]);
                di += 32;
            }
            best = Some((
                block,
                Val::Account {
                    nonce: p_nonce,
                    balance: p_val,
                    code_hash: p_code,
                },
            ));
        }
        j += 1;
    }
    Ok(match best {
        Some((b, v)) => (Look::Found(b, v), Next::PageEnd),
        None => (Look::AfterLast, Next::PageEnd),
    })
}

/// Every record of a raw page, in order.
pub(crate) fn for_each(
    page: &[u8],
    storage: bool,
    mut f: impl FnMut(&[u8], u64, Val),
) -> Result<(), Error> {
    let Streams {
        n,
        kinds,
        keys,
        blocks,
        tags,
        nonce,
        vals,
        codes,
        ..
    } = Streams::parse(page, storage)?;
    let r = if storage { R_STORAGE } else { R_ACCOUNT };
    let (mut kc, mut addr_off, mut key_off) = (0usize, 0usize, 0usize);
    let (mut bi, mut vi, mut ni, mut di) = (0usize, 0usize, 0usize, 0usize);
    let mut block = 0u64;
    let mut p_val = [0u8; 32];
    let mut p_nonce = 0u64;
    let mut p_code = KECCAK_EMPTY;
    let mut full = [0u8; 64];
    for j in 0..n {
        let tag = tags[j];
        let new_key = j == 0
            || if storage {
                kinds[j] != K_SAME
            } else {
                tag & T_NEW != 0
            };
        if new_key {
            if storage && (j == 0 || kinds[j] == K_ADDR) {
                addr_off = kc;
                kc += 32;
            }
            key_off = kc;
            kc += 32;
            block = get_varint(blocks, &mut bi);
            p_val = [0; 32];
            p_nonce = 0;
            p_code = KECCAK_EMPTY;
        } else {
            block += get_varint(blocks, &mut bi);
        }
        if j % r == 0 {
            p_val = [0; 32];
            p_nonce = 0;
        }
        let key: &[u8] = if storage {
            full[..32].copy_from_slice(&keys[addr_off..addr_off + 32]);
            full[32..].copy_from_slice(&keys[key_off..key_off + 32]);
            &full
        } else {
            &keys[key_off..key_off + 32]
        };
        if storage {
            let len = (tag & 63) as usize;
            p_val = delta_decode(&p_val, tag, &vals[vi..vi + len]);
            vi += len;
            f(key, block, Val::Storage(p_val));
        } else if tag & T_DELETED != 0 {
            p_nonce = 0;
            p_val = [0; 32];
            p_code = KECCAK_EMPTY;
            f(key, block, Val::Deleted);
        } else {
            if tag & T_NONCE != 0 {
                let x = get_varint(nonce, &mut ni);
                p_nonce = if tag & T_NONCE_ABS != 0 {
                    x
                } else {
                    p_nonce + x
                };
            }
            if tag & T_BALANCE != 0 {
                let t = vals[vi];
                let len = (t & 63) as usize;
                p_val = delta_decode(&p_val, t, &vals[vi + 1..vi + 1 + len]);
                vi += 1 + len;
            }
            if tag & T_CODE != 0 {
                p_code.copy_from_slice(&codes[di..di + 32]);
                di += 32;
            }
            f(
                key,
                block,
                Val::Account {
                    nonce: p_nonce,
                    balance: p_val,
                    code_hash: p_code,
                },
            );
        }
    }
    Ok(())
}

/// The first record's key and block, and the record count, of a raw page.
pub(crate) fn first_record(page: &[u8], storage: bool) -> Result<([u8; 64], u64, usize), Error> {
    let s = Streams::parse(page, storage)?;
    let k = key_len(storage);
    if s.keys.len() < k || s.blocks.is_empty() {
        return Err(corrupt("malformed page"));
    }
    let mut key = [0u8; 64];
    key[..k].copy_from_slice(&s.keys[..k]);
    let mut pos = 0;
    let block = get_varint(s.blocks, &mut pos);
    Ok((key, block, s.n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    fn word(seed: &mut u64, small: bool) -> [u8; 32] {
        let mut w = [0u8; 32];
        let n = if small { 1 + rng(seed) % 9 } else { 32 } as usize;
        for b in w[32 - n..].iter_mut() {
            *b = rng(seed) as u8;
        }
        w
    }

    /// Sorted random versions: clustered keys, several versions per key.
    pub(crate) fn sample(storage: bool, keys: usize, seed: u64) -> Vec<([u8; 64], u64, Val)> {
        let mut seed = seed | 1;
        let k = key_len(storage);
        let mut ks: Vec<[u8; 64]> = (0..keys)
            .map(|i| {
                let mut key = [0u8; 64];
                // Few addresses, many slots each.
                let a = if storage { (i / 7) as u64 } else { i as u64 };
                let mut s = a.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
                for b in key[..32].iter_mut() {
                    *b = rng(&mut s) as u8;
                }
                if storage {
                    for b in key[32..64].iter_mut() {
                        *b = rng(&mut seed) as u8;
                    }
                }
                key
            })
            .collect();
        ks.sort();
        ks.dedup_by(|a, b| a[..k] == b[..k]);
        let mut out = Vec::new();
        for key in ks {
            let versions = 1 + rng(&mut seed) % 5 + if rng(&mut seed) % 50 == 0 { 300 } else { 0 };
            let mut block = rng(&mut seed) % 1000;
            let mut prev = word(&mut seed, true);
            let mut nonce = 0u64;
            for _ in 0..versions {
                block += 1 + rng(&mut seed) % 1000;
                let val = if storage {
                    prev = match rng(&mut seed) % 4 {
                        0 => word(&mut seed, false),
                        1 => [0; 32],
                        2 => add256(&prev, &word(&mut seed, true)),
                        _ => word(&mut seed, true),
                    };
                    Val::Storage(prev)
                } else if rng(&mut seed) % 20 == 0 {
                    nonce = 0;
                    Val::Deleted
                } else {
                    nonce = match rng(&mut seed) % 8 {
                        0 => nonce / 2,
                        1 => nonce,
                        _ => nonce + 1,
                    };
                    prev = add256(&prev, &word(&mut seed, true));
                    Val::Account {
                        nonce,
                        balance: prev,
                        code_hash: if rng(&mut seed) % 4 == 0 {
                            word(&mut seed, false)
                        } else {
                            KECCAK_EMPTY
                        },
                    }
                };
                out.push((key, block, val));
            }
        }
        out
    }

    /// Encode `recs` into pages of about `target` bytes.
    fn pages(storage: bool, recs: &[([u8; 64], u64, Val)], target: usize) -> Vec<(usize, Vec<u8>)> {
        let k = key_len(storage);
        let mut enc = PageEnc::new(storage);
        let mut out = Vec::new();
        let mut first = 0;
        for (i, (key, block, val)) in recs.iter().enumerate() {
            if enc.len() > 0 && enc.size() >= target {
                let mut page = Vec::new();
                enc.finish(&mut page);
                out.push((first, page));
                first = i;
            }
            enc.add(&key[..k], *block, val);
        }
        let mut page = Vec::new();
        enc.finish(&mut page);
        out.push((first, page));
        out
    }

    #[test]
    fn delta_round_trips() {
        let mut seed = 7;
        for _ in 0..2000 {
            let (sa, sb) = (rng(&mut seed) % 2 == 0, rng(&mut seed) % 2 == 0);
            let (a, b) = (word(&mut seed, sa), word(&mut seed, sb));
            for (p, c) in [(a, b), (b, a), (a, a), ([0; 32], a), (a, [0xff; 32])] {
                let mut bytes = Vec::new();
                let tag = delta_encode(&p, &c, &mut bytes);
                assert_eq!((tag & 63) as usize, bytes.len());
                assert_eq!(delta_decode(&p, tag, &bytes), c);
            }
        }
    }

    #[test]
    fn pages_round_trip_and_look_up() {
        for storage in [false, true] {
            let k = key_len(storage);
            let recs = sample(storage, 600, 42);
            for target in [300, 2000, PAGE_TARGET] {
                let pages = pages(storage, &recs, target);
                let mut all = Vec::new();
                for (first, page) in &pages {
                    let (fk, fb, n) = first_record(page, storage).unwrap();
                    assert_eq!((&fk[..k], fb), (&recs[*first].0[..k], recs[*first].1));
                    let before = all.len();
                    for_each(page, storage, |key, block, val| {
                        let mut kk = [0u8; 64];
                        kk[..k].copy_from_slice(key);
                        all.push((kk, block, val));
                    })
                    .unwrap();
                    assert_eq!(all.len() - before, n);
                }
                assert_eq!(all, recs);

                // Every version, just below it, and far above, in its page.
                for (pi, (first, page)) in pages.iter().enumerate() {
                    let end = pages.get(pi + 1).map_or(recs.len(), |p| p.0);
                    for i in *first..end {
                        let (key, block, val) = &recs[i];
                        let key = &key[..k];
                        let next_same = recs.get(i + 1).filter(|r| r.0[..k] == *key);
                        let (look, next) = lookup(page, storage, key, *block).unwrap();
                        assert_eq!(look, Look::Found(*block, *val));
                        let want = if i + 1 == end {
                            Next::PageEnd
                        } else {
                            next_same.map_or(Next::None, |r| Next::At(r.1))
                        };
                        assert_eq!(next, want);
                        // At the height before this version: the previous one,
                        // which may be on an earlier page.
                        let prev = (i > 0 && recs[i - 1].0[..k] == *key).then(|| &recs[i - 1]);
                        let (look, next) = lookup(page, storage, key, *block - 1).unwrap();
                        match prev {
                            Some(p) if i > *first => {
                                assert_eq!(look, Look::Found(p.1, p.2));
                                assert_eq!(next, Next::At(*block));
                            }
                            Some(_) => assert_eq!(look, Look::BeforeFirst),
                            None if i > *first => {
                                assert_eq!(look, Look::Absent);
                                assert_eq!(next, Next::At(*block));
                            }
                            None => assert_eq!(look, Look::BeforeFirst),
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn absent_keys() {
        let recs = sample(true, 300, 9);
        let pages = pages(true, &recs, 4000);
        let (first, page) = &pages[1];
        let end = pages[2].0;
        // A key between two keys of the page.
        let mut probe = recs[*first + 3].0;
        probe[63] ^= 1;
        if !recs[*first..end].iter().any(|r| r.0 == probe) {
            let (look, next) = lookup(page, true, &probe, u32::MAX as u64).unwrap();
            assert!(matches!(look, Look::Absent | Look::AfterLast), "{look:?}");
            if look == Look::Absent {
                assert_eq!(next, Next::None);
            }
        }
        assert_eq!(
            lookup(page, true, &[0u8; 64], 5).unwrap().0,
            Look::BeforeFirst
        );
        assert_eq!(
            lookup(page, true, &[0xffu8; 64], 5).unwrap().0,
            Look::AfterLast
        );
    }

    #[test]
    fn malformed_pages_are_errors() {
        let recs = sample(false, 50, 3);
        let page = pages(false, &recs, PAGE_TARGET).remove(0).1;
        assert!(lookup(&page[..page.len() / 2], false, &recs[0].0[..32], 9).is_err());
        assert!(for_each(&[], false, |_, _, _| {}).is_err());
        let mut bad = page.clone();
        bad[0] ^= 0x15;
        assert!(for_each(&bad, false, |_, _, _| {}).is_err());
    }
}
