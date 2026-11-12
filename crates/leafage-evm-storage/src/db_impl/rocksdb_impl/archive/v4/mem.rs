//! Memtable of archive schema v4: the versions of the blocks not yet written
//! to a segment, readable at any height.
//!
//! Versions are appended to an arena per shard and chained newest-first per
//! key; a key with many versions also gets a sorted index. Blocks arrive in
//! order, so a key's chain is sorted by block.

use super::codec::{get_varint, put_varint, strip, Val};
use super::corrupt;
use super::side::BlockRec;
use crate::db_impl::error::Error;
use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

const NONE: u32 = u32::MAX;
const SHARDS: usize = 64;
/// Versions after which a key gets a sorted (block, offset) index.
const HOT: u32 = 32;

/// Keys are keccak hashes: hash them by two of their own words.
#[derive(Default, Clone, Copy)]
pub(crate) struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, b: &[u8]) {
        let n = b.len();
        if n >= 16 {
            self.0 ^= u64::from_le_bytes(b[..8].try_into().unwrap())
                ^ u64::from_le_bytes(b[n - 8..].try_into().unwrap()).rotate_left(29);
        } else {
            for &x in b {
                self.0 = self.0.rotate_left(8) ^ x as u64;
            }
        }
    }
    fn write_usize(&mut self, _: usize) {}
}

#[derive(Default, Clone, Copy)]
pub(crate) struct KeyHashBuild;

impl BuildHasher for KeyHashBuild {
    type Hasher = KeyHasher;
    fn build_hasher(&self) -> KeyHasher {
        KeyHasher(0)
    }
}

/// A key's versions in a shard.
#[derive(Clone, Copy)]
struct KeyHead {
    /// Arena offset of the newest version.
    newest: u32,
    newest_block: u32,
    oldest_block: u32,
    count: u32,
    /// Index into `Shard::hot`, or `NONE`.
    hot: u32,
}

/// One shard: versions appended to an arena, each linked to the key's
/// previous version. Record: `prev u32 | block u32 | kind u8`, then for a
/// slot `len u8 | value without leading zeros`, for an account
/// `nonce varint | len u8 | balance | code index u32`, nothing for a
/// deleted account.
#[derive(Default)]
struct Shard {
    acct: HashMap<[u8; 32], KeyHead, KeyHashBuild>,
    stor: HashMap<[u8; 64], KeyHead, KeyHashBuild>,
    arena: Vec<u8>,
    codes: Vec<[u8; 32]>,
    code_ix: HashMap<[u8; 32], u32, KeyHashBuild>,
    hot: Vec<Vec<(u32, u32)>>,
}

const K_STORAGE: u8 = 0;
const K_DELETED: u8 = 1;
const K_ACCOUNT: u8 = 2;

impl Shard {
    fn bytes(&self) -> u64 {
        (self.arena.capacity()
            + self.acct.capacity() * (32 + 20 + 1)
            + self.stor.capacity() * (64 + 20 + 1)
            + self.codes.capacity() * 32
            + self.code_ix.capacity() * (32 + 4 + 1)
            + self
                .hot
                .iter()
                .map(|v| v.capacity() * 8 + 24)
                .sum::<usize>()) as u64
    }

    fn head_mut(&mut self, storage: bool, key: &[u8; 64]) -> Option<&mut KeyHead> {
        if storage {
            self.stor.get_mut(key)
        } else {
            self.acct
                .get_mut(<&[u8; 32]>::try_from(&key[..32]).unwrap())
        }
    }

    fn head(&self, storage: bool, key: &[u8]) -> Option<&KeyHead> {
        if storage {
            self.stor.get(<&[u8; 64]>::try_from(key).unwrap())
        } else {
            self.acct.get(<&[u8; 32]>::try_from(key).unwrap())
        }
    }

    fn insert(&mut self, storage: bool, key: &[u8; 64], block: u64, v: &Val) -> Result<(), Error> {
        let off = self.arena.len();
        if off >= (NONE - 256) as usize {
            return Err(corrupt("memtable shard over 4 GiB"));
        }
        let block32 = block as u32;
        let fresh = KeyHead {
            newest: NONE,
            newest_block: 0,
            oldest_block: block32,
            count: 0,
            hot: NONE,
        };
        let head = if storage {
            self.stor.entry(*key).or_insert(fresh)
        } else {
            self.acct
                .entry(key[..32].try_into().unwrap())
                .or_insert(fresh)
        };
        // The same block again (a commit retried): the new value replaces
        // the one written before.
        let replace = head.newest != NONE && head.newest_block == block32;
        if head.newest != NONE && head.newest_block > block32 {
            return Err(corrupt(format!(
                "memtable write at block {block} below the key's newest {}",
                head.newest_block
            )));
        }
        let old = head.newest;
        head.newest = off as u32;
        head.newest_block = block32;
        if !replace {
            head.count += 1;
        }
        let (count, hot) = (head.count, head.hot);
        let prev = if replace {
            u32::from_le_bytes(
                self.arena[old as usize..old as usize + 4]
                    .try_into()
                    .unwrap(),
            )
        } else {
            old
        };
        self.arena.extend_from_slice(&prev.to_le_bytes());
        self.arena.extend_from_slice(&block32.to_le_bytes());
        match v {
            Val::Storage(x) => {
                self.arena.push(K_STORAGE);
                let x = strip(x);
                self.arena.push(x.len() as u8);
                self.arena.extend_from_slice(x);
            }
            Val::Deleted => self.arena.push(K_DELETED),
            Val::Account {
                nonce,
                balance,
                code_hash,
            } => {
                self.arena.push(K_ACCOUNT);
                put_varint(&mut self.arena, *nonce);
                let x = strip(balance);
                self.arena.push(x.len() as u8);
                self.arena.extend_from_slice(x);
                let n = self.codes.len() as u32;
                let ix = *self.code_ix.entry(*code_hash).or_insert(n);
                if ix == n {
                    self.codes.push(*code_hash);
                }
                self.arena.extend_from_slice(&ix.to_le_bytes());
            }
        }
        if hot != NONE {
            let idx = &mut self.hot[hot as usize];
            if replace {
                idx.pop();
            }
            idx.push((block32, off as u32));
        } else if count == HOT && !replace {
            // Index the chain once; later versions are appended.
            let mut idx = Vec::with_capacity(2 * HOT as usize);
            let mut o = off as u32;
            loop {
                let (p, b, _) = self.decode(o);
                idx.push((b as u32, o));
                if p == NONE {
                    break;
                }
                o = p;
            }
            idx.reverse();
            self.hot.push(idx);
            let h = (self.hot.len() - 1) as u32;
            self.head_mut(storage, key).expect("just inserted").hot = h;
        }
        Ok(())
    }

    /// The record at `off`: its key's previous record, its block and value.
    fn decode(&self, off: u32) -> (u32, u64, Val) {
        let a = &self.arena;
        let mut p = off as usize;
        let prev = u32::from_le_bytes(a[p..p + 4].try_into().unwrap());
        let block = u32::from_le_bytes(a[p + 4..p + 8].try_into().unwrap()) as u64;
        let kind = a[p + 8];
        p += 9;
        let pad = |b: &[u8]| {
            let mut o = [0u8; 32];
            o[32 - b.len()..].copy_from_slice(b);
            o
        };
        let v = match kind {
            K_STORAGE => {
                let l = a[p] as usize;
                Val::Storage(pad(&a[p + 1..p + 1 + l]))
            }
            K_DELETED => Val::Deleted,
            _ => {
                let nonce = get_varint(a, &mut p);
                let l = a[p] as usize;
                let balance = pad(&a[p + 1..p + 1 + l]);
                p += 1 + l;
                let ix = u32::from_le_bytes(a[p..p + 4].try_into().unwrap());
                Val::Account {
                    nonce,
                    balance,
                    code_hash: self.codes[ix as usize],
                }
            }
        };
        (prev, block, v)
    }

    fn get(&self, storage: bool, key: &[u8], h: u64) -> MemRead {
        let Some(head) = self.head(storage, key) else {
            return MemRead::NONE;
        };
        if h < head.oldest_block as u64 {
            return MemRead {
                found: None,
                next: head.oldest_block as u64,
            };
        }
        if h >= head.newest_block as u64 {
            let (_, block, v) = self.decode(head.newest);
            return MemRead {
                found: Some((block, v)),
                next: u64::MAX,
            };
        }
        if head.hot != NONE {
            let idx = &self.hot[head.hot as usize];
            let i = idx.partition_point(|&(b, _)| b as u64 <= h);
            let (_, block, v) = self.decode(idx[i - 1].1);
            return MemRead {
                found: Some((block, v)),
                next: idx[i].0 as u64,
            };
        }
        // The oldest version is <= h, so the chain does not end first.
        let mut off = head.newest;
        let mut next = u64::MAX;
        loop {
            let (prev, block, v) = self.decode(off);
            if block <= h {
                return MemRead {
                    found: Some((block, v)),
                    next,
                };
            }
            next = block;
            off = prev;
        }
    }
}

/// A memtable read: the greatest version `<= h` of the key here, and the
/// block of the key's next version here after it (`u64::MAX`: none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemRead {
    pub(crate) found: Option<(u64, Val)>,
    pub(crate) next: u64,
}

impl MemRead {
    const NONE: Self = Self {
        found: None,
        next: u64::MAX,
    };
}

/// The memtable: independently locked shards, chosen by a key byte.
pub(crate) struct Mem {
    shards: Vec<RwLock<Shard>>,
    versions: AtomicU64,
    /// First and last block applied, with or without versions; the first is
    /// `u64::MAX` while there is none.
    first: AtomicU64,
    last: AtomicU64,
    /// The infos of the blocks applied, by number.
    blocks: RwLock<BTreeMap<u64, BlockRec>>,
    /// Code first seen in these blocks, by hash.
    codes: RwLock<HashMap<[u8; 32], Arc<[u8]>>>,
}

impl Default for Mem {
    fn default() -> Self {
        Self {
            shards: (0..SHARDS).map(|_| RwLock::new(Shard::default())).collect(),
            versions: AtomicU64::new(0),
            first: AtomicU64::new(u64::MAX),
            last: AtomicU64::new(0),
            blocks: RwLock::default(),
            codes: RwLock::default(),
        }
    }
}

impl Mem {
    #[inline]
    fn shard(key: &[u8]) -> usize {
        key[8] as usize % SHARDS
    }

    pub(crate) fn versions(&self) -> u64 {
        self.versions.load(Ordering::Acquire)
    }

    /// Whether no block was applied.
    pub(crate) fn is_empty(&self) -> bool {
        self.first.load(Ordering::Acquire) == u64::MAX
    }

    /// A block was applied (one writer at a time).
    pub(crate) fn note_block(&self, block: u64) {
        self.first.fetch_min(block, Ordering::AcqRel);
        self.last.fetch_max(block, Ordering::AcqRel);
    }

    /// The first and last block applied.
    pub(crate) fn span(&self) -> Option<(u64, u64)> {
        let first = self.first.load(Ordering::Acquire);
        (first != u64::MAX).then(|| (first, self.last.load(Ordering::Acquire)))
    }

    pub(crate) fn put_block(&self, number: u64, rec: BlockRec) {
        self.note_block(number);
        self.blocks.write().unwrap().insert(number, rec);
    }

    pub(crate) fn block(&self, number: u64) -> Option<BlockRec> {
        self.blocks.read().unwrap().get(&number).cloned()
    }

    /// The blocks with an info in `[from, to)`, in order.
    pub(crate) fn blocks_in(&self, from: u64, to: u64) -> Vec<(u64, BlockRec)> {
        if from >= to {
            return Vec::new();
        }
        self.blocks
            .read()
            .unwrap()
            .range(from..to)
            .map(|(n, rec)| (*n, rec.clone()))
            .collect()
    }

    pub(crate) fn put_code(&self, hash: [u8; 32], code: Arc<[u8]>) {
        self.codes.write().unwrap().insert(hash, code);
    }

    pub(crate) fn code(&self, hash: &[u8; 32]) -> Option<Arc<[u8]>> {
        self.codes.read().unwrap().get(hash).cloned()
    }

    pub(crate) fn codes(&self) -> Vec<([u8; 32], Arc<[u8]>)> {
        let mut codes: Vec<_> = self
            .codes
            .read()
            .unwrap()
            .iter()
            .map(|(hash, code)| (*hash, code.clone()))
            .collect();
        codes.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        codes
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.shards.iter().map(|s| s.read().unwrap().bytes()).sum()
    }

    /// Distinct keys of one kind.
    pub(crate) fn keys(&self, storage: bool) -> usize {
        self.shards
            .iter()
            .map(|s| {
                let g = s.read().unwrap();
                if storage {
                    g.stor.len()
                } else {
                    g.acct.len()
                }
            })
            .sum()
    }

    /// Add a version. One writer at a time, blocks in order; the same block
    /// may be written again and then replaces what it wrote.
    pub(crate) fn insert(
        &self,
        storage: bool,
        key: &[u8; 64],
        block: u64,
        v: &Val,
    ) -> Result<(), Error> {
        self.shards[Self::shard(key)]
            .write()
            .unwrap()
            .insert(storage, key, block, v)?;
        self.versions.fetch_add(1, Ordering::AcqRel);
        self.note_block(block);
        Ok(())
    }

    #[inline]
    pub(crate) fn get(&self, storage: bool, key: &[u8], h: u64) -> MemRead {
        if self.versions.load(Ordering::Relaxed) == 0 {
            return MemRead::NONE;
        }
        self.shards[Self::shard(key)]
            .read()
            .unwrap()
            .get(storage, key, h)
    }

    /// The slots of `address` with a version here.
    pub(crate) fn slots_of(&self, address: &[u8; 32]) -> Vec<[u8; 32]> {
        if self.is_empty() {
            return Vec::new();
        }
        let g = self.shards[Self::shard(address)].read().unwrap();
        g.stor
            .keys()
            .filter(|k| k[..32] == address[..])
            .map(|k| k[32..].try_into().unwrap())
            .collect()
    }

    /// Every version of one kind in (key, block) order.
    pub(crate) fn for_each_sorted(
        &self,
        storage: bool,
        mut f: impl FnMut(&[u8], u64, &Val) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let guards: Vec<_> = self.shards.iter().map(|s| s.read().unwrap()).collect();
        let mut heads: Vec<([u8; 64], u8, u32)> = Vec::new();
        for (si, g) in guards.iter().enumerate() {
            if storage {
                heads.extend(g.stor.iter().map(|(k, h)| (*k, si as u8, h.newest)));
            } else {
                heads.extend(g.acct.iter().map(|(k, h)| {
                    let mut kk = [0u8; 64];
                    kk[..32].copy_from_slice(k);
                    (kk, si as u8, h.newest)
                }));
            }
        }
        heads.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let k = if storage { 64 } else { 32 };
        let mut chain: Vec<u32> = Vec::new();
        for (key, si, newest) in heads {
            let g = &guards[si as usize];
            chain.clear();
            let mut off = newest;
            loop {
                chain.push(off);
                let (prev, _, _) = g.decode(off);
                if prev == NONE {
                    break;
                }
                off = prev;
            }
            for &o in chain.iter().rev() {
                let (_, block, v) = g.decode(o);
                f(&key[..k], block, &v)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn key(a: u8, s: u8) -> [u8; 64] {
        let mut k = [0u8; 64];
        k[..32].fill(a);
        k[8] = a.wrapping_mul(7);
        k[32..].fill(s);
        k
    }

    fn slot(v: u64) -> Val {
        let mut w = [0u8; 32];
        w[24..].copy_from_slice(&v.to_be_bytes());
        Val::Storage(w)
    }

    #[test]
    fn reads_at_any_height_with_next_version() {
        let mem = Mem::default();
        let mut model: BTreeMap<([u8; 64], u64), Val> = BTreeMap::new();
        let mut seed = 5u64;
        let mut rng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for block in 100..400u64 {
            for _ in 0..6 {
                // One hot key, a few warm ones.
                let k = if rng() % 3 == 0 {
                    key(1, 1)
                } else {
                    key((rng() % 5) as u8, (rng() % 4) as u8)
                };
                let v = slot(rng() % 1000);
                mem.insert(true, &k, block, &v).unwrap();
                model.insert((k, block), v);
            }
            if block % 50 == 0 {
                let mut a = key(9, 0);
                a[32..].fill(0);
                let v = if block % 100 == 0 {
                    Val::Deleted
                } else {
                    Val::Account {
                        nonce: block,
                        balance: [7; 32],
                        code_hash: [block as u8; 32],
                    }
                };
                mem.insert(false, &a, block, &v).unwrap();
            }
        }
        assert_eq!(mem.span(), Some((100, 399)));
        for a in 0..5u8 {
            for s in 0..4u8 {
                let k = key(a, s);
                for h in (90..410u64).step_by(3) {
                    let want = model
                        .range((k, 0)..=(k, h))
                        .next_back()
                        .map(|(kb, v)| (kb.1, *v));
                    let next = model
                        .range((k, h + 1)..=(k, u64::MAX))
                        .next()
                        .map_or(u64::MAX, |(kb, _)| kb.1);
                    let got = mem.get(true, &k, h);
                    assert_eq!(got.found, want, "key {a}/{s} at {h}");
                    assert_eq!(got.next, next, "next of {a}/{s} at {h}");
                }
            }
        }
        assert_eq!(mem.get(true, &key(77, 0), 300), MemRead::NONE);
        let mut a = key(9, 0);
        a[32..].fill(0);
        assert_eq!(
            mem.get(false, &a[..32], 249).found,
            Some((200, Val::Deleted))
        );
        assert_eq!(mem.get(false, &a[..32], 249).next, 250);

        // Sorted iteration yields exactly the model.
        let mut seen = Vec::new();
        mem.for_each_sorted(true, |k, b, v| {
            seen.push((<[u8; 64]>::try_from(k).unwrap(), b, *v));
            Ok(())
        })
        .unwrap();
        let want: Vec<_> = model.iter().map(|((k, b), v)| (*k, *b, *v)).collect();
        assert_eq!(seen, want);
        assert_eq!(mem.keys(true), 20);
        let mut slots = mem.slots_of(&key(1, 0)[..32].try_into().unwrap());
        slots.sort();
        assert_eq!(slots.len(), 4);
    }

    #[test]
    fn a_block_written_again_replaces_its_versions() {
        let mem = Mem::default();
        let k = key(3, 3);
        for round in 0..3u64 {
            for block in 1..=40u64 {
                // Rewrite every block of the last round's tail.
                if round > 0 && block < 40 {
                    continue;
                }
                mem.insert(true, &k, block, &slot(block * 10 + round))
                    .unwrap();
            }
        }
        assert_eq!(mem.get(true, &k, 40).found, Some((40, slot(402))));
        assert_eq!(mem.get(true, &k, 39).found, Some((39, slot(390))));
        assert_eq!(mem.get(true, &k, 39).next, 40);
        let mut n = 0;
        mem.for_each_sorted(true, |_, _, _| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 40);
        // An older block is refused.
        assert!(mem.insert(true, &k, 12, &slot(1)).is_err());
    }
}
