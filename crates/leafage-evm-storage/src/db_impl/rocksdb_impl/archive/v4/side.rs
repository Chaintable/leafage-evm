//! Block infos and contract code of a schema v4 archive: what is neither an
//! account nor a storage version.
//!
//! Both are append-only files whose valid length the store's manifest
//! records, so they commit with the segments of the same flush. Bytes past
//! that length are leftovers of an interrupted flush and are cut off at
//! open.
//!
//! They are read through the kernel's page cache, unlike page files: they
//! are appended to while they are read, and direct reads do not mix with
//! buffered writes of the same file.
//!
//! Block infos, by number:
//!
//! - `blocks.dat`: chunks of [`BLOCK_CHUNK`] consecutive blocks, each a zstd
//!   frame (with checksum) of `hash 32 | length u32 LE | info` per block. A
//!   block that was never written has a zero hash and no info.
//! - `blocks.off`: where each full chunk is, `offset << 24 | length` as u64
//!   LE. The last chunk is in it once it is full; until then the manifest
//!   says where it is, and the next append writes it again, longer, after
//!   it.
//! - `blocks.pfx`: the first 8 bytes of the hash of each block, at
//!   `(number - first) * 8`: what the index from hash to number is built
//!   from, without reading the chunks. 0: never written.
//! - `blocks.dict`: the zstd dictionary of the chunks, if the manifest says
//!   there is one. It is chosen once, from block infos like those to come;
//!   chunks written before it need none.
//!
//! Code, by hash:
//!
//! - `code.dat`: `hash 32 | zstd frame` per code.
//! - `code.idx`: `hash 32 | offset u64 LE | length u32 LE` per code, in the
//!   order written. An open reads all of it into a table sorted by the
//!   first 8 bytes of the hash, 16 bytes per code.
//! - `code.dict`: the zstd dictionary of the codes, as `blocks.dict`.
//!   Contracts repeat each other: with it a code takes two thirds of what
//!   it takes alone.

use super::corrupt;
use super::file::{decompress_frame, sync_dir, write_file_synced, Dict};
use crate::db_impl::error::Error;
use quick_cache::sync::Cache as QuickCache;
use quick_cache::Weighter;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use zstd::zstd_safe;

/// Blocks per chunk of `blocks.dat`: about 16 KiB of bsc block infos.
pub(crate) const BLOCK_CHUNK: u64 = 16;
/// zstd level of what is written in bulk; see also [`LEVEL_SLOW`].
pub(crate) const LEVEL: i32 = 3;
/// zstd level of what a node following the chain writes: megabytes a day.
pub(crate) const LEVEL_SLOW: i32 = 19;
/// A chunk or a code is at most this long, compressed...
const MAX_LEN: u64 = (1 << 24) - 1;
/// ...and this long raw.
const MAX_RAW: usize = 1 << 28;
/// And `blocks.dat` at most this long.
const MAX_OFFSET: u64 = (1 << 40) - 1;
/// Bytes of the dictionary of block chunks, and of code.
const BLOCKS_DICT_BYTES: usize = 32 << 10;
#[cfg(not(test))]
const CODE_DICT_BYTES: usize = 1 << 20;
#[cfg(test)]
const CODE_DICT_BYTES: usize = 16 << 10;
/// Codes the files must hold before a dictionary is trained from them, and
/// about how many of them it is trained on.
#[cfg(not(test))]
const CODE_DICT_AFTER: u64 = 20_000;
#[cfg(test)]
const CODE_DICT_AFTER: u64 = 600;
const CODE_DICT_SAMPLES: usize = 8_000;

const BLOCKS_DAT: &str = "blocks.dat";
const BLOCKS_OFF: &str = "blocks.off";
const BLOCKS_PFX: &str = "blocks.pfx";
const BLOCKS_DICT: &str = "blocks.dict";
const CODE_DAT: &str = "code.dat";
const CODE_IDX: &str = "code.idx";
const CODE_DICT: &str = "code.dict";
const CODE_ENTRY: u64 = 44;

fn pack(offset: u64, len: u64) -> Result<u64, Error> {
    if offset > MAX_OFFSET || len > MAX_LEN {
        return Err(Error::UnSupported(format!(
            "archive v4 block or code record of {len} bytes at offset {offset}"
        )));
    }
    Ok(offset << 24 | len)
}

fn unpack(packed: u64) -> (u64, usize) {
    (packed >> 24, (packed & MAX_LEN) as usize)
}

fn open_rw(path: &Path, len: u64) -> Result<File, Error> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let have = file.metadata()?.len();
    if have < len {
        return Err(corrupt(format!(
            "{} has {have} bytes, the manifest says {len}",
            path.display()
        )));
    }
    if have > len {
        file.set_len(len)?;
        file.sync_all()?;
    }
    Ok(file)
}

/// The valid part of the block files, as the manifest records it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BlocksMeta {
    /// The first block held, meaningful once `count > 0`.
    pub(crate) first: u64,
    pub(crate) count: u64,
    /// Bytes of `blocks.dat`.
    pub(crate) dat_len: u64,
    /// Where the last chunk is while it is not full; 0 otherwise.
    pub(crate) tail: u64,
    /// Whether chunks are written with the dictionary in `blocks.dict`.
    pub(crate) dict: bool,
}

impl BlocksMeta {
    /// The last block held.
    pub(crate) fn last(&self) -> Option<u64> {
        (self.count > 0).then(|| self.first + self.count - 1)
    }

    fn full_chunks(&self) -> u64 {
        self.count / BLOCK_CHUNK
    }
}

/// One block: its hash and encoded info. A zero hash: never written.
pub(crate) type BlockRec = ([u8; 32], Arc<[u8]>);

fn chunk_bytes(blocks: &[BlockRec]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(blocks.iter().map(|b| 36 + b.1.len()).sum());
    for (hash, info) in blocks {
        raw.extend_from_slice(hash);
        raw.extend_from_slice(&(info.len() as u32).to_le_bytes());
        raw.extend_from_slice(info);
    }
    raw
}

fn decode_chunk(raw: &[u8]) -> Result<Vec<BlockRec>, Error> {
    let bad = || corrupt("malformed archive v4 block chunk");
    let mut out = Vec::with_capacity(BLOCK_CHUNK as usize);
    let mut at = 0usize;
    while at < raw.len() {
        let hash: [u8; 32] = raw.get(at..at + 32).ok_or_else(bad)?.try_into().unwrap();
        let len = u32::from_le_bytes(
            raw.get(at + 32..at + 36)
                .ok_or_else(bad)?
                .try_into()
                .unwrap(),
        ) as usize;
        let info = raw.get(at + 36..at + 36 + len).ok_or_else(bad)?;
        out.push((hash, Arc::from(info)));
        at += 36 + len;
    }
    Ok(out)
}

/// A compressor of frames with a checksum, for `dict` if it is not empty.
fn compressor(level: i32, dict: &Dict) -> Result<zstd::bulk::Compressor<'static>, Error> {
    let mut comp = if dict.raw.is_empty() {
        zstd::bulk::Compressor::new(level)?
    } else {
        zstd::bulk::Compressor::with_dictionary(level, &dict.raw)?
    };
    comp.set_parameter(zstd_safe::CParameter::ChecksumFlag(true))?;
    Ok(comp)
}

/// Train a dictionary of at most `bytes` on `samples`; empty when there is
/// too little to learn from.
pub(crate) fn train_dict(samples: &[&[u8]], bytes: usize) -> Vec<u8> {
    let total: usize = samples.iter().map(|s| s.len()).sum();
    if samples.len() < 256 || total < 8 * bytes {
        return Vec::new();
    }
    zstd::dict::from_samples(samples, bytes).unwrap_or_default()
}

/// The dictionary `name` of a store: installed by renaming, read at open.
fn write_dict(dir: &Path, name: &str, raw: &[u8]) -> Result<(), Error> {
    let tmp = dir.join(format!("{name}.tmp"));
    write_file_synced(&tmp, raw)?;
    std::fs::rename(&tmp, dir.join(name))?;
    sync_dir(dir)
}

#[derive(Clone, Copy)]
struct ChunkWeighter;

impl Weighter<u64, Arc<Vec<BlockRec>>> for ChunkWeighter {
    fn weight(&self, _: &u64, chunk: &Arc<Vec<BlockRec>>) -> u64 {
        chunk.iter().map(|b| 64 + b.1.len() as u64).sum::<u64>() + 64
    }
}

struct BlocksState {
    meta: BlocksMeta,
    /// Of the full chunks.
    offsets: Vec<u64>,
    /// Empty while the files have none.
    dict: Arc<Dict>,
}

/// An append to the block files that is written and synced, and not yet
/// part of them: see [`Blocks::publish`].
pub(crate) struct BlocksAppend {
    pub(crate) meta: BlocksMeta,
    offsets: Vec<u64>,
    /// Full chunks kept from before.
    keep: usize,
    /// The dictionary this append brings.
    dict: Option<Arc<Dict>>,
}

/// The block infos of an archive, by number.
pub(crate) struct Blocks {
    dir: PathBuf,
    dat: File,
    off: File,
    pfx: File,
    state: RwLock<BlocksState>,
    cache: QuickCache<u64, Arc<Vec<BlockRec>>, ChunkWeighter>,
}

impl Blocks {
    pub(crate) fn open(dir: &Path, meta: BlocksMeta, cache_bytes: u64) -> Result<Self, Error> {
        let dat = open_rw(&dir.join(BLOCKS_DAT), meta.dat_len)?;
        let off = open_rw(&dir.join(BLOCKS_OFF), meta.full_chunks() * 8)?;
        let pfx = open_rw(&dir.join(BLOCKS_PFX), meta.count * 8)?;
        let mut bytes = vec![0u8; (meta.full_chunks() * 8) as usize];
        off.read_exact_at(&mut bytes, 0)?;
        let offsets = bytes
            .chunks_exact(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
            .collect();
        let dict = if meta.dict {
            std::fs::read(dir.join(BLOCKS_DICT))?
        } else {
            Vec::new()
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            dat,
            off,
            pfx,
            state: RwLock::new(BlocksState {
                meta,
                offsets,
                dict: Arc::new(Dict::new(0, dict)),
            }),
            cache: QuickCache::with_weighter(
                (cache_bytes / (BLOCK_CHUNK * 1024)).max(64) as usize,
                cache_bytes.max(1 << 20),
                ChunkWeighter,
            ),
        })
    }

    pub(crate) fn meta(&self) -> BlocksMeta {
        self.state.read().unwrap().meta
    }

    /// Bytes of the table of chunk offsets.
    pub(crate) fn mem_bytes(&self) -> u64 {
        self.state.read().unwrap().offsets.len() as u64 * 8
    }

    fn read_chunk(&self, packed: u64, dict: &Dict) -> Result<Vec<BlockRec>, Error> {
        let (offset, len) = unpack(packed);
        let mut frame = vec![0u8; len];
        self.dat.read_exact_at(&mut frame, offset)?;
        decode_chunk(&decompress_frame(
            &frame,
            dict,
            MAX_RAW,
            "archive v4 block chunk",
        )?)
    }

    /// The chunk block `number` is in and the block's place in it, if the
    /// files hold the block.
    fn chunk_of(&self, number: u64) -> Result<Option<(Arc<Vec<BlockRec>>, usize)>, Error> {
        let (index, packed, cached, dict) = {
            let st = self.state.read().unwrap();
            let Some(i) = number
                .checked_sub(st.meta.first)
                .filter(|i| *i < st.meta.count)
            else {
                return Ok(None);
            };
            let chunk = i / BLOCK_CHUNK;
            match st.offsets.get(chunk as usize) {
                Some(&packed) => (i % BLOCK_CHUNK, packed, Some(chunk), st.dict.clone()),
                // The last chunk changes with every append: not cached.
                None => (i % BLOCK_CHUNK, st.meta.tail, None, st.dict.clone()),
            }
        };
        let chunk = match cached.and_then(|c| self.cache.get(&c)) {
            Some(chunk) => chunk,
            None => {
                let chunk = Arc::new(self.read_chunk(packed, &dict)?);
                if let Some(c) = cached {
                    self.cache.insert(c, chunk.clone());
                }
                chunk
            }
        };
        if chunk.len() <= index as usize {
            return Err(corrupt(format!(
                "archive v4 block chunk lacks block {number}"
            )));
        }
        Ok(Some((chunk, index as usize)))
    }

    /// The hash and info of block `number`; `None` if the files do not hold
    /// it or it was never written.
    pub(crate) fn get(&self, number: u64) -> Result<Option<BlockRec>, Error> {
        Ok(self
            .chunk_of(number)?
            .map(|(chunk, i)| chunk[i].clone())
            .filter(|rec| rec.0 != [0u8; 32]))
    }

    /// The first 8 bytes of the hashes of the blocks in `[from, to)` that
    /// the files hold, in order; 0 for a block never written.
    pub(crate) fn scan_prefixes(
        &self,
        from: u64,
        to: u64,
        f: &mut dyn FnMut(u64, u64) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let meta = self.meta();
        let from = from.max(meta.first);
        let to = to.min(meta.first + meta.count);
        let mut buf = vec![0u8; 1 << 20];
        let mut number = from;
        while number < to {
            let n = ((to - number) * 8).min(buf.len() as u64) as usize;
            self.pfx
                .read_exact_at(&mut buf[..n], (number - meta.first) * 8)?;
            for prefix in buf[..n].chunks_exact(8) {
                f(number, u64::from_be_bytes(prefix.try_into().unwrap()))?;
                number += 1;
            }
        }
        Ok(())
    }

    /// A dictionary for files that have none: the chunks written from now
    /// on use it. `infos` are block infos like those to come. As
    /// [`Self::append`], in two steps; `None` when there is too little to
    /// learn from, or the files have one.
    pub(crate) fn train(&self, infos: &[&[u8]]) -> Result<Option<BlocksAppend>, Error> {
        let (meta, keep) = {
            let st = self.state.read().unwrap();
            (st.meta, st.offsets.len())
        };
        if meta.dict {
            return Ok(None);
        }
        let raw = train_dict(infos, BLOCKS_DICT_BYTES);
        if raw.is_empty() {
            return Ok(None);
        }
        write_dict(&self.dir, BLOCKS_DICT, &raw)?;
        Ok(Some(BlocksAppend {
            meta: BlocksMeta { dict: true, ..meta },
            offsets: Vec::new(),
            keep,
            dict: Some(Arc::new(Dict::new(0, raw))),
        }))
    }

    /// Write `blocks`, those after the last one held (or from `first` on in
    /// empty files), compressed at `level`, and sync them. They are part of
    /// the files once the manifest records the result and
    /// [`Self::publish`] has it.
    pub(crate) fn append(
        &self,
        first: u64,
        blocks: &[BlockRec],
        level: i32,
    ) -> Result<BlocksAppend, Error> {
        let (meta, keep, dict) = {
            let st = self.state.read().unwrap();
            (st.meta, st.offsets.len(), st.dict.clone())
        };
        if blocks.is_empty() {
            return Ok(BlocksAppend {
                meta,
                offsets: Vec::new(),
                keep,
                dict: None,
            });
        }
        if meta.count > 0 && first != meta.first + meta.count {
            return Err(Error::UnSupported(format!(
                "archive v4 holds blocks up to {}, block {first} does not follow",
                meta.first + meta.count - 1
            )));
        }
        // The blocks of the last chunk, if it is not full, come first: it is
        // written again with what follows.
        let mut pending: Vec<BlockRec> = if meta.tail != 0 {
            self.read_chunk(meta.tail, &dict)?
        } else {
            Vec::new()
        };
        if pending.len() as u64 != meta.count % BLOCK_CHUNK {
            return Err(corrupt("archive v4 last block chunk has another length"));
        }
        pending.extend_from_slice(blocks);
        let mut comp = compressor(level, &dict)?;
        let mut at = meta.dat_len;
        let mut out = Vec::new();
        let mut offsets = Vec::new();
        let mut tail = 0;
        for chunk in pending.chunks(BLOCK_CHUNK as usize) {
            let frame = comp.compress(&chunk_bytes(chunk))?;
            let packed = pack(at + out.len() as u64, frame.len() as u64)?;
            if chunk.len() as u64 == BLOCK_CHUNK {
                offsets.push(packed);
            } else {
                tail = packed;
            }
            out.extend_from_slice(&frame);
            if out.len() >= 8 << 20 {
                self.dat.write_all_at(&out, at)?;
                at += out.len() as u64;
                out.clear();
            }
        }
        self.dat.write_all_at(&out, at)?;
        at += out.len() as u64;
        let off_bytes: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
        self.off.write_all_at(&off_bytes, keep as u64 * 8)?;
        let mut prefixes = Vec::with_capacity(blocks.len() * 8);
        for b in blocks {
            prefixes.extend_from_slice(&b.0[..8]);
        }
        self.pfx.write_all_at(&prefixes, meta.count * 8)?;
        self.dat.sync_data()?;
        self.off.sync_data()?;
        self.pfx.sync_data()?;
        Ok(BlocksAppend {
            meta: BlocksMeta {
                first: if meta.count == 0 { first } else { meta.first },
                count: meta.count + blocks.len() as u64,
                dat_len: at,
                tail,
                dict: meta.dict,
            },
            offsets,
            keep,
            dict: None,
        })
    }

    /// Cut the files back to end at block `last`, which they hold. As
    /// [`Self::append`], in two steps.
    pub(crate) fn cut(&self, last: u64) -> Result<BlocksAppend, Error> {
        let meta = self.meta();
        let count = match last.checked_sub(meta.first) {
            Some(n) if n < meta.count => n + 1,
            _ => {
                return Err(Error::UnSupported(format!(
                    "archive v4 block files do not hold block {last}"
                )))
            }
        };
        let keep = (count / BLOCK_CHUNK) as usize;
        let rest = (count % BLOCK_CHUNK) as usize;
        let mut new = BlocksMeta {
            count,
            tail: 0,
            ..meta
        };
        if rest > 0 {
            // The blocks of the chunk `last` is in, up to it, as a new last
            // chunk after everything else.
            let (packed, dict) = {
                let st = self.state.read().unwrap();
                (
                    st.offsets.get(keep).copied().unwrap_or(st.meta.tail),
                    st.dict.clone(),
                )
            };
            let mut blocks = self.read_chunk(packed, &dict)?;
            blocks.truncate(rest);
            let frame = compressor(LEVEL, &dict)?.compress(&chunk_bytes(&blocks))?;
            self.dat.write_all_at(&frame, meta.dat_len)?;
            self.dat.sync_data()?;
            new.tail = pack(meta.dat_len, frame.len() as u64)?;
            new.dat_len += frame.len() as u64;
        }
        Ok(BlocksAppend {
            meta: new,
            offsets: Vec::new(),
            keep,
            dict: None,
        })
    }

    /// Make an append (or a cut) that the manifest now records part of the
    /// files.
    pub(crate) fn publish(&self, append: BlocksAppend) -> Result<(), Error> {
        let mut st = self.state.write().unwrap();
        let cut = append.meta.count < st.meta.count;
        st.offsets.truncate(append.keep);
        st.offsets.extend_from_slice(&append.offsets);
        st.meta = append.meta;
        if let Some(dict) = append.dict {
            st.dict = dict;
        }
        if cut {
            // What the files held after it must not come back with a later
            // append of other blocks.
            self.cache.clear();
            self.off.set_len(st.offsets.len() as u64 * 8)?;
            self.pfx.set_len(st.meta.count * 8)?;
        }
        Ok(())
    }
}

/// The valid part of the code files, as the manifest records it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CodesMeta {
    pub(crate) count: u64,
    /// Bytes of `code.dat`.
    pub(crate) dat_len: u64,
    /// Whether codes are written with the dictionary in `code.dict`.
    pub(crate) dict: bool,
}

struct CodesState {
    meta: CodesMeta,
    /// First 8 bytes of the hash and where the code is, sorted.
    sorted: Vec<(u64, u64)>,
    /// Those added since the table was sorted last.
    recent: HashMap<u64, Vec<u64>>,
    recent_len: usize,
    /// Empty while the files have none.
    dict: Arc<Dict>,
}

/// An append to the code files, as [`BlocksAppend`].
pub(crate) struct CodesAppend {
    pub(crate) meta: CodesMeta,
    entries: Vec<(u64, u64)>,
    dict: Option<Arc<Dict>>,
}

/// Contract code by hash.
pub(crate) struct Codes {
    dir: PathBuf,
    dat: File,
    idx: File,
    state: RwLock<CodesState>,
}

fn prefix(hash: &[u8; 32]) -> u64 {
    u64::from_be_bytes(hash[..8].try_into().unwrap())
}

/// Compress each of `codes` with `dict` at `level`, on several threads when
/// that pays.
fn compress_codes(codes: &[&[u8]], dict: &Dict, level: i32) -> Result<Vec<Vec<u8>>, Error> {
    let threads = if level > 9 && codes.len() >= 64 {
        std::thread::available_parallelism().map_or(1, |n| n.get().min(16))
    } else {
        1
    };
    let per = codes.len().div_ceil(threads).max(1);
    let parts: Vec<Result<Vec<Vec<u8>>, Error>> = std::thread::scope(|scope| {
        let workers: Vec<_> = codes
            .chunks(per)
            .map(|part| {
                scope.spawn(move || -> Result<Vec<Vec<u8>>, Error> {
                    let mut comp = compressor(level, dict)?;
                    part.iter().map(|code| Ok(comp.compress(code)?)).collect()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("code compressor panicked"))
            .collect()
    });
    let mut out = Vec::with_capacity(codes.len());
    for part in parts {
        out.extend(part?);
    }
    Ok(out)
}

impl Codes {
    pub(crate) fn open(dir: &Path, meta: CodesMeta) -> Result<Self, Error> {
        let dat = open_rw(&dir.join(CODE_DAT), meta.dat_len)?;
        let idx = open_rw(&dir.join(CODE_IDX), meta.count * CODE_ENTRY)?;
        let mut sorted = Vec::with_capacity(meta.count as usize);
        let mut buf = vec![0u8; (CODE_ENTRY as usize) << 14];
        let mut at = 0u64;
        while at < meta.count * CODE_ENTRY {
            let n = (meta.count * CODE_ENTRY - at).min(buf.len() as u64) as usize;
            idx.read_exact_at(&mut buf[..n], at)?;
            for e in buf[..n].chunks_exact(CODE_ENTRY as usize) {
                let offset = u64::from_le_bytes(e[32..40].try_into().unwrap());
                let len = u32::from_le_bytes(e[40..44].try_into().unwrap());
                sorted.push((
                    prefix(e[..32].try_into().unwrap()),
                    pack(offset, len as u64)?,
                ));
            }
            at += n as u64;
        }
        sorted.sort_unstable();
        let dict = if meta.dict {
            std::fs::read(dir.join(CODE_DICT))?
        } else {
            Vec::new()
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            dat,
            idx,
            state: RwLock::new(CodesState {
                meta,
                sorted,
                recent: HashMap::new(),
                recent_len: 0,
                dict: Arc::new(Dict::new(0, dict)),
            }),
        })
    }

    pub(crate) fn meta(&self) -> CodesMeta {
        self.state.read().unwrap().meta
    }

    /// Bytes of the table.
    pub(crate) fn mem_bytes(&self) -> u64 {
        let st = self.state.read().unwrap();
        (st.sorted.len() + 2 * st.recent_len) as u64 * 16 + st.dict.raw.len() as u64
    }

    fn candidates(&self, hash: &[u8; 32]) -> (Vec<u64>, Arc<Dict>) {
        let p = prefix(hash);
        let st = self.state.read().unwrap();
        let from = st.sorted.partition_point(|e| e.0 < p);
        let mut out: Vec<u64> = st.sorted[from..]
            .iter()
            .take_while(|e| e.0 == p)
            .map(|e| e.1)
            .collect();
        if let Some(recent) = st.recent.get(&p) {
            out.extend_from_slice(recent);
        }
        (out, st.dict.clone())
    }

    /// A record as stored: the hash, then the frame of the code.
    fn read_stored(&self, packed: u64) -> Result<Vec<u8>, Error> {
        let (offset, len) = unpack(packed);
        let mut record = vec![0u8; len];
        self.dat.read_exact_at(&mut record, offset)?;
        if record.len() < 32 {
            return Err(corrupt("short archive v4 code record"));
        }
        Ok(record)
    }

    fn read_record(&self, packed: u64, dict: &Dict) -> Result<([u8; 32], Vec<u8>), Error> {
        let record = self.read_stored(packed)?;
        let code = decompress_frame(&record[32..], dict, MAX_RAW, "archive v4 code")?;
        Ok((record[..32].try_into().unwrap(), code))
    }

    /// The code with hash `hash`.
    pub(crate) fn get(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>, Error> {
        let (candidates, dict) = self.candidates(hash);
        for packed in candidates {
            // One read for the hash and the code: a candidate is the code
            // asked for, unless two hashes start with the same 8 bytes.
            let record = self.read_stored(packed)?;
            if record[..32] == *hash {
                let frame = &record[32..];
                return decompress_frame(frame, &dict, MAX_RAW, "archive v4 code").map(Some);
            }
            if record[..8] != hash[..8] {
                // The index said this place has a hash with these bytes.
                return Err(corrupt("archive v4 code index points at another code"));
            }
        }
        Ok(None)
    }

    /// Whether the files hold the code with hash `hash`.
    pub(crate) fn contains(&self, hash: &[u8; 32]) -> Result<bool, Error> {
        for packed in self.candidates(hash).0 {
            let (offset, _) = unpack(packed);
            let mut stored = [0u8; 32];
            self.dat.read_exact_at(&mut stored, offset)?;
            if stored == *hash {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A dictionary for files that have none, from `codes`: the codes
    /// written from now on use it. As [`Self::append`], in two steps;
    /// `None` when there is too little to learn from, or the files have
    /// one.
    pub(crate) fn train(&self, codes: &[&[u8]]) -> Result<Option<CodesAppend>, Error> {
        let meta = self.meta();
        if meta.dict {
            return Ok(None);
        }
        let raw = train_dict(codes, CODE_DICT_BYTES);
        if raw.is_empty() {
            return Ok(None);
        }
        write_dict(&self.dir, CODE_DICT, &raw)?;
        Ok(Some(CodesAppend {
            meta: CodesMeta { dict: true, ..meta },
            entries: Vec::new(),
            dict: Some(Arc::new(Dict::new(0, raw))),
        }))
    }

    /// [`Self::train`] on codes the files hold, once they hold enough: for
    /// an archive that began without code to learn from.
    pub(crate) fn train_from_stored(&self) -> Result<Option<CodesAppend>, Error> {
        let (picked, dict) = {
            let st = self.state.read().unwrap();
            if st.meta.dict || st.meta.count < CODE_DICT_AFTER {
                return Ok(None);
            }
            // Of every code held, in the order written: those added since
            // the table was sorted last are most of a young archive.
            let mut all: Vec<u64> = st
                .sorted
                .iter()
                .map(|e| e.1)
                .chain(st.recent.values().flatten().copied())
                .collect();
            all.sort_unstable();
            let stride = (all.len() / CODE_DICT_SAMPLES).max(1);
            let picked: Vec<u64> = all.into_iter().step_by(stride).collect();
            (picked, st.dict.clone())
        };
        let mut codes = Vec::with_capacity(picked.len());
        for packed in picked {
            codes.push(self.read_record(packed, &dict)?.1);
        }
        let codes: Vec<&[u8]> = codes.iter().map(Vec::as_slice).collect();
        self.train(&codes)
    }

    /// Write the codes the files do not hold yet, compressed at `level`,
    /// and sync them; see [`Blocks::append`].
    pub(crate) fn append<'a>(
        &self,
        codes: impl IntoIterator<Item = (&'a [u8; 32], &'a [u8])>,
        level: i32,
    ) -> Result<CodesAppend, Error> {
        let (meta, dict) = {
            let st = self.state.read().unwrap();
            (st.meta, st.dict.clone())
        };
        let mut seen: HashMap<[u8; 32], ()> = HashMap::new();
        let mut new: Vec<(&[u8; 32], &[u8])> = Vec::new();
        for (hash, code) in codes {
            if seen.insert(*hash, ()).is_none() && !self.contains(hash)? {
                new.push((hash, code));
            }
        }
        if new.is_empty() {
            return Ok(CodesAppend {
                meta,
                entries: Vec::new(),
                dict: None,
            });
        }
        let raw: Vec<&[u8]> = new.iter().map(|(_, code)| *code).collect();
        let frames = compress_codes(&raw, &dict, level)?;
        let mut at = meta.dat_len;
        let (mut out, mut idx) = (Vec::new(), Vec::new());
        let mut entries: Vec<(u64, u64)> = Vec::with_capacity(new.len());
        for ((hash, _), frame) in new.iter().zip(&frames) {
            let offset = at + out.len() as u64;
            let len = 32 + frame.len() as u64;
            entries.push((prefix(hash), pack(offset, len)?));
            out.extend_from_slice(*hash);
            out.extend_from_slice(frame);
            idx.extend_from_slice(*hash);
            idx.extend_from_slice(&offset.to_le_bytes());
            idx.extend_from_slice(&(len as u32).to_le_bytes());
            if out.len() >= 8 << 20 {
                self.dat.write_all_at(&out, at)?;
                at += out.len() as u64;
                out.clear();
            }
        }
        self.dat.write_all_at(&out, at)?;
        at += out.len() as u64;
        self.idx.write_all_at(&idx, meta.count * CODE_ENTRY)?;
        self.dat.sync_data()?;
        self.idx.sync_data()?;
        Ok(CodesAppend {
            meta: CodesMeta {
                count: meta.count + entries.len() as u64,
                dat_len: at,
                dict: meta.dict,
            },
            entries,
            dict: None,
        })
    }

    /// Make an append that the manifest now records part of the files.
    pub(crate) fn publish(&self, append: CodesAppend) {
        let mut st = self.state.write().unwrap();
        st.meta = append.meta;
        if let Some(dict) = append.dict {
            st.dict = dict;
        }
        st.recent_len += append.entries.len();
        for (p, packed) in append.entries {
            st.recent.entry(p).or_default().push(packed);
        }
        if st.recent_len >= 1 << 16 {
            let recent = std::mem::take(&mut st.recent);
            st.sorted.extend(
                recent
                    .into_iter()
                    .flat_map(|(p, v)| v.into_iter().map(move |e| (p, e))),
            );
            st.sorted.sort_unstable();
            st.recent_len = 0;
        }
    }
}

/// Create the (empty) files in a new store directory.
pub(crate) fn create(dir: &Path) -> Result<(), Error> {
    for name in [BLOCKS_DAT, BLOCKS_OFF, BLOCKS_PFX, CODE_DAT, CODE_IDX] {
        File::create(dir.join(name))?.sync_all()?;
    }
    sync_dir(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("leafage-v4-side-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        create(&dir).unwrap();
        dir
    }

    fn block(n: u64) -> BlockRec {
        if n % 11 == 5 {
            // Never written.
            return ([0u8; 32], Arc::from(&[][..]));
        }
        let mut hash = [0x5au8; 32];
        hash[..8].copy_from_slice(&n.to_be_bytes());
        let info: Vec<u8> = (0..(n % 300) as usize + 1)
            .map(|i| (i as u64 * n) as u8)
            .collect();
        (hash, Arc::from(info))
    }

    fn check(blocks: &Blocks, first: u64, last: u64) {
        assert_eq!(blocks.meta().last(), Some(last));
        assert_eq!(blocks.get(first - 1).unwrap(), None);
        assert_eq!(blocks.get(last + 1).unwrap(), None);
        for n in first..=last {
            let want = block(n);
            let got = blocks.get(n).unwrap();
            assert_eq!(
                got,
                (want.0 != [0u8; 32]).then_some(want.clone()),
                "block {n}"
            );
        }
        let mut seen = Vec::new();
        blocks
            .scan_prefixes(0, u64::MAX, &mut |n, prefix| {
                seen.push((n, prefix));
                Ok(())
            })
            .unwrap();
        let want: Vec<_> = (first..=last)
            .map(|n| (n, u64::from_be_bytes(block(n).0[..8].try_into().unwrap())))
            .collect();
        assert_eq!(seen, want);
    }

    #[test]
    fn blocks_append_reopen_and_cut() {
        let dir = dir("blocks");
        let mut meta = BlocksMeta::default();
        let first = 1000u64;
        let mut next = first;
        // Appends of many sizes: chunks filled, left open, extended.
        for (round, n) in [1u64, 3, 12, 1, 16, 40, 7, 0, 25].into_iter().enumerate() {
            let blocks = Blocks::open(&dir, meta, 1 << 20).unwrap();
            let recs: Vec<BlockRec> = (next..next + n).map(block).collect();
            let append = blocks.append(next, &recs, LEVEL).unwrap();
            // Not there before it is published.
            if n > 0 {
                assert_eq!(blocks.get(next).unwrap(), None);
            }
            meta = append.meta;
            blocks.publish(append).unwrap();
            next += n;
            check(&blocks, first, next - 1);
            if round % 2 == 0 {
                // An append the manifest never recorded is cut off.
                let lost = blocks
                    .append(next, &[block(next), block(next + 1)], LEVEL)
                    .unwrap();
                assert_eq!(lost.meta.count, meta.count + 2);
            }
        }
        // A block that does not follow is refused.
        let blocks = Blocks::open(&dir, meta, 1 << 20).unwrap();
        assert!(blocks.append(next + 1, &[block(next + 1)], LEVEL).is_err());
        check(&blocks, first, next - 1);
        // A cut to a block the files do not hold is refused.
        for last in [first - 1, 0, next, u64::MAX] {
            assert!(blocks.cut(last).is_err(), "cut to {last}");
        }
        // Cuts: into a chunk, onto a chunk's end, then appends go on.
        for last in [first + 70, first + 63, first + 2] {
            let cut = blocks.cut(last).unwrap();
            meta = cut.meta;
            blocks.publish(cut).unwrap();
            check(&blocks, first, last);
            let blocks = Blocks::open(&dir, meta, 1 << 20).unwrap();
            check(&blocks, first, last);
        }
        let recs: Vec<BlockRec> = (first + 3..first + 50).map(block).collect();
        let append = blocks.append(first + 3, &recs, LEVEL).unwrap();
        meta = append.meta;
        blocks.publish(append).unwrap();
        check(&blocks, first, first + 49);
        check(
            &Blocks::open(&dir, meta, 1 << 20).unwrap(),
            first,
            first + 49,
        );
        // A dictionary comes once there are blocks to learn from; chunks
        // written before and after it are read alike.
        assert!(blocks.train(&[&recs[0].1[..]]).unwrap().is_none());
        let samples: Vec<BlockRec> = (0..4000u64).map(|n| block(1_000_000 + n)).collect();
        let infos: Vec<&[u8]> = samples.iter().map(|rec| &rec.1[..]).collect();
        let dict = blocks.train(&infos).unwrap().expect("enough to learn from");
        assert!(dict.meta.dict && !meta.dict);
        blocks.publish(dict).unwrap();
        assert!(blocks.train(&infos).unwrap().is_none());
        let recs: Vec<BlockRec> = (first + 50..first + 200).map(block).collect();
        let append = blocks.append(first + 50, &recs, LEVEL_SLOW).unwrap();
        meta = append.meta;
        assert!(meta.dict);
        blocks.publish(append).unwrap();
        check(&blocks, first, first + 199);
        check(
            &Blocks::open(&dir, meta, 1 << 20).unwrap(),
            first,
            first + 199,
        );
        // A shorter file than the manifest says is damage.
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join(BLOCKS_PFX))
            .unwrap()
            .set_len(32)
            .unwrap();
        assert!(Blocks::open(&dir, meta, 1 << 20).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codes_append_and_reopen() {
        let dir = dir("codes");
        // Contracts made of shared pieces, as real ones are.
        let piece = |p: u64| -> Vec<u8> {
            (0..200u64)
                .map(|b| (b.wrapping_mul(p * 2 + 1).wrapping_add(p >> 3)) as u8)
                .collect()
        };
        let code = |i: u64| -> ([u8; 32], Vec<u8>) {
            let mut hash = [0u8; 32];
            // Pairs that share their first 8 bytes.
            hash[..8].copy_from_slice(&(i / 2).to_be_bytes());
            hash[31] = i as u8;
            hash[30] = (i >> 8) as u8;
            let mut bytes = Vec::new();
            for j in 0..(i % 9) {
                bytes.extend(piece((i * 7 + j * 13) % 40));
            }
            bytes.extend_from_slice(&i.to_le_bytes()[..(i % 5) as usize]);
            (hash, bytes)
        };
        let check = |codes: &Codes, written: u64| {
            for i in 0..written {
                let (hash, bytes) = code(i);
                assert_eq!(codes.get(&hash).unwrap(), Some(bytes), "code {i}");
                assert!(codes.contains(&hash).unwrap());
            }
            assert_eq!(codes.get(&code(written + 5).0).unwrap(), None);
            assert!(!codes.contains(&[0xff; 32]).unwrap());
        };
        let mut meta = CodesMeta::default();
        let mut written = 0u64;
        for (n, level) in [(5u64, LEVEL), (0, LEVEL), (300, LEVEL_SLOW), (1, LEVEL)] {
            let codes = Codes::open(&dir, meta).unwrap();
            let batch: Vec<_> = (written.saturating_sub(2)..written + n).map(code).collect();
            let append = codes
                .append(batch.iter().map(|(h, c)| (h, c.as_slice())), level)
                .unwrap();
            // Those already held are not written again.
            assert_eq!(append.meta.count, meta.count + n);
            if n > 0 {
                assert_eq!(codes.get(&code(written).0).unwrap(), None);
            }
            meta = append.meta;
            codes.publish(append);
            written += n;
            check(&codes, written);
            check(&Codes::open(&dir, meta).unwrap(), written);
        }
        // A dictionary comes from the codes held, once they are many; codes
        // written before and after it are read alike.
        let codes = Codes::open(&dir, meta).unwrap();
        assert!(codes.train_from_stored().unwrap().is_none());
        let batch: Vec<_> = (written..written + 700).map(code).collect();
        let append = codes
            .append(batch.iter().map(|(h, c)| (h, c.as_slice())), LEVEL)
            .unwrap();
        let alone = append.meta.dat_len - meta.dat_len;
        meta = append.meta;
        codes.publish(append);
        written += 700;
        let dict = codes
            .train_from_stored()
            .unwrap()
            .expect("enough to learn from");
        assert!(dict.meta.dict && dict.meta.count == meta.count);
        meta = dict.meta;
        codes.publish(dict);
        assert!(codes.train_from_stored().unwrap().is_none());
        let batch: Vec<_> = (written..written + 700).map(code).collect();
        let append = codes
            .append(batch.iter().map(|(h, c)| (h, c.as_slice())), LEVEL)
            .unwrap();
        let with_dict = append.meta.dat_len - meta.dat_len;
        assert!(
            with_dict < alone,
            "{with_dict} bytes with the dictionary, {alone} without"
        );
        meta = append.meta;
        codes.publish(append);
        written += 700;
        check(&codes, written);
        check(&Codes::open(&dir, meta).unwrap(), written);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample_code(i: u64) -> ([u8; 32], Vec<u8>) {
        let mut hash = [0x5au8; 32];
        hash[..8].copy_from_slice(&i.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
        let bytes = (0..400 + i % 300)
            .map(|b| (b.wrapping_mul(i % 37 * 2 + 1) >> 2) as u8)
            .collect();
        (hash, bytes)
    }

    /// An archive that was never reopened has its codes in the part of the
    /// table not sorted yet: the dictionary is learnt from them too.
    #[test]
    fn the_code_dictionary_is_learnt_from_codes_just_written() {
        let dir = dir("codes-young");
        let codes = Codes::open(&dir, CodesMeta::default()).unwrap();
        let batch: Vec<_> = (0..CODE_DICT_AFTER + 100).map(sample_code).collect();
        let append = codes
            .append(batch.iter().map(|(h, c)| (h, c.as_slice())), LEVEL)
            .unwrap();
        codes.publish(append);
        let dict = codes
            .train_from_stored()
            .unwrap()
            .expect("the codes written are enough to learn from");
        assert!(dict.meta.dict);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A code is not taken from a place that holds another: an index entry
    /// that points elsewhere is an error, not a missing code.
    #[test]
    fn a_code_index_that_points_elsewhere_is_an_error() {
        let dir = dir("side-damage");
        let codes = Codes::open(&dir, CodesMeta::default()).unwrap();
        let batch: Vec<_> = (0..50).map(sample_code).collect();
        let append = codes
            .append(batch.iter().map(|(h, c)| (h, c.as_slice())), LEVEL)
            .unwrap();
        let meta = append.meta;
        codes.publish(append);
        drop(codes);
        // The offset of the fourth code.
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join(CODE_IDX))
            .unwrap();
        let at = 3 * CODE_ENTRY + 33;
        let mut b = [0u8; 1];
        f.read_exact_at(&mut b, at).unwrap();
        f.write_all_at(&[b[0] ^ 0x10], at).unwrap();
        let codes = Codes::open(&dir, meta).unwrap();
        assert!(codes.get(&sample_code(3).0).is_err());
        assert_eq!(
            codes.get(&sample_code(4).0).unwrap(),
            Some(sample_code(4).1)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
