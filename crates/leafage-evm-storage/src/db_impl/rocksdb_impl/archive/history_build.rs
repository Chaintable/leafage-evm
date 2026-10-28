//! Offline tools for archive schema v3 (`docs/ArchiveSchemaV3.md`): build
//! the first sealed epoch from a v2 (inverted) archive, and verify sealed
//! segments against it.

use super::{decode_archive_header, StorageTypeColumn, ENCODING_MARKER_KEY};
use crate::db_impl::error::Error;
use crate::db_impl::history_seg::{
    shard_of, strip_leading_zeros, HistoryStore, Manifest, ManifestEpoch, ManifestFile, SegKind,
    SegmentStats, SegmentWriter,
};
use leafage_evm_types::{H256, U256};
use rocksdb::{Options, ReadOptions, DB};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};

fn cf_name(kind: SegKind) -> &'static str {
    match kind {
        SegKind::Account => StorageTypeColumn::AddressToAccount.to_str(),
        SegKind::Storage => StorageTypeColumn::AddressToStorage.to_str(),
    }
}

fn io_err(e: std::io::Error) -> Error {
    Error::UnSupported(format!("history segment I/O: {e}"))
}

/// Open a v2 archive read-only, checking it is inverted.
fn open_v2_read_only(path: &Path) -> Result<DB, Error> {
    let mut opts = Options::default();
    opts.set_max_open_files(
        std::env::var("ROCKSDB_MAX_OPEN_FILE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4096),
    );
    let cf_names = [
        StorageTypeColumn::LatestBlockHash.to_str(),
        StorageTypeColumn::BlockHashToBlockInfo.to_str(),
        StorageTypeColumn::BlockNumToBlockHash.to_str(),
        StorageTypeColumn::AddressToAccount.to_str(),
        StorageTypeColumn::AddressToStorage.to_str(),
        StorageTypeColumn::HashToCode.to_str(),
    ];
    let db = DB::open_cf_for_read_only(&opts, path, cf_names, false).map_err(Error::RocksDB)?;
    let cf = db
        .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
        .unwrap();
    let marker = db
        .get_cf(cf, ENCODING_MARKER_KEY)
        .map_err(Error::RocksDB)?
        .and_then(|v| v.first().copied());
    if marker != Some(1) {
        return Err(Error::UnSupported(
            "v3 build needs a v2 (inverted) archive; this one is not marked inverted".into(),
        ));
    }
    Ok(db)
}

/// The archive head: the number of the latest block.
fn read_head(db: &DB) -> Result<u64, Error> {
    let cf = db
        .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
        .unwrap();
    let hash = db
        .get_cf(cf, [1u8])
        .map_err(Error::RocksDB)?
        .ok_or_else(|| Error::UnSupported("archive has no latest block".into()))?;
    let cf = db
        .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
        .unwrap();
    let info = db
        .get_cf(cf, &hash)
        .map_err(Error::RocksDB)?
        .ok_or_else(|| Error::UnSupported("latest block has no block info".into()))?;
    Ok(decode_archive_header(&mut info.as_slice())?.number)
}

/// Iteration options for one shard's key range.
fn shard_read_options(shard: usize, shard_bits: u8, readahead: usize) -> ReadOptions {
    let mut ro = ReadOptions::default();
    ro.set_verify_checksums(false);
    ro.set_total_order_seek(true);
    ro.fill_cache(false);
    ro.set_readahead_size(readahead);
    if shard_bits > 0 {
        let width = 1usize << (8 - shard_bits);
        ro.set_iterate_lower_bound(vec![(shard * width) as u8]);
        if (shard + 1) * width < 256 {
            ro.set_iterate_upper_bound(vec![((shard + 1) * width) as u8]);
        }
    }
    ro
}

/// Decode an inverted v2 version tail.
#[inline]
fn tail_block(tail: &[u8]) -> u64 {
    u64::MAX - u64::from_be_bytes(tail[24..32].try_into().unwrap())
}

/// Versions of one key, buffered newest-first as the v2 CF yields them.
#[derive(Default)]
struct KeyRun {
    key: Vec<u8>,
    blocks: Vec<u64>,
    values: Vec<u8>,
    ends: Vec<usize>,
}

impl KeyRun {
    fn push(&mut self, block: u64, value: &[u8]) {
        self.blocks.push(block);
        self.values.extend_from_slice(value);
        self.ends.push(self.values.len());
    }

    /// Emit the run oldest-first.
    fn drain_into(&mut self, w: &mut SegmentWriter) -> std::io::Result<u64> {
        let n = self.blocks.len();
        for i in (0..n).rev() {
            let start = if i == 0 { 0 } else { self.ends[i - 1] };
            w.push(&self.key, self.blocks[i], &self.values[start..self.ends[i]])?;
        }
        self.blocks.clear();
        self.values.clear();
        self.ends.clear();
        Ok(n as u64)
    }
}

struct Progress {
    records: AtomicU64,
    skipped: AtomicU64,
    done_jobs: AtomicUsize,
}

#[allow(clippy::too_many_arguments)]
fn build_shard(
    src: &DB,
    kind: SegKind,
    shard: usize,
    shard_bits: u8,
    head: u64,
    path: &Path,
    readahead: usize,
    progress: &Progress,
) -> Result<SegmentStats, Error> {
    let k = kind.key_len();
    let cf = src.cf_handle(cf_name(kind)).unwrap();
    let mut it = src.raw_iterator_cf_opt(cf, shard_read_options(shard, shard_bits, readahead));
    it.seek_to_first();
    let mut w = SegmentWriter::create(path, kind, shard as u16, 0, head).map_err(io_err)?;
    let mut run = KeyRun::default();
    let (mut pending, mut skipped) = (0u64, 0u64);
    while let (Some(key), Some(value)) = (it.key(), it.value()) {
        if key.len() != k + 32 {
            return Err(Error::UnSupported(format!(
                "{}: unexpected key length {}",
                kind.name(),
                key.len()
            )));
        }
        debug_assert_eq!(shard_of(key, shard_bits), shard);
        let block = tail_block(&key[k..]);
        if block > head {
            skipped += 1;
        } else {
            if run.key.as_slice() != &key[..k] {
                pending += run.drain_into(&mut w).map_err(io_err)?;
                run.key.clear();
                run.key.extend_from_slice(&key[..k]);
            }
            let value = match kind {
                SegKind::Storage => {
                    if value.len() > 32 {
                        return Err(Error::UnSupported("storage value over 32 bytes".into()));
                    }
                    strip_leading_zeros(value)
                }
                SegKind::Account => value,
            };
            run.push(block, value);
        }
        if pending >= 1 << 20 {
            progress.records.fetch_add(pending, Ordering::Relaxed);
            pending = 0;
        }
        it.next();
    }
    it.status().map_err(Error::RocksDB)?;
    pending += run.drain_into(&mut w).map_err(io_err)?;
    progress.records.fetch_add(pending, Ordering::Relaxed);
    progress.skipped.fetch_add(skipped, Ordering::Relaxed);
    w.finish().map_err(io_err)
}

/// Build epoch 0 (`[0, head]`) of a v3 history from the v2 archive at
/// `src_path` into `out_dir`, with `jobs` parallel shard builders. The
/// archive is opened read-only and left untouched.
pub fn build_history_v3(
    src_path: &Path,
    out_dir: &Path,
    jobs: usize,
    shard_bits: u8,
    readahead: usize,
) -> Result<Manifest, Error> {
    if shard_bits > 8 {
        return Err(Error::UnSupported("shard_bits must be <= 8".into()));
    }
    let src = open_v2_read_only(src_path)?;
    let head = read_head(&src)?;
    let dir = format!("epoch-0-{head}");
    // Never write into a directory a manifest may already point at: a
    // serving node maps those files.
    std::fs::create_dir_all(out_dir).map_err(io_err)?;
    std::fs::create_dir(out_dir.join(&dir)).map_err(|e| {
        Error::UnSupported(format!(
            "{}: {e} (refusing to overwrite segments)",
            out_dir.join(&dir).display()
        ))
    })?;
    let shards = 1usize << shard_bits;
    // Storage first: it is the bulk of the work.
    let tasks: Vec<(SegKind, usize)> = [SegKind::Storage, SegKind::Account]
        .into_iter()
        .flat_map(|kind| (0..shards).map(move |s| (kind, s)))
        .collect();
    info!(target: "history_v3",
        "building epoch 0 [0, {head}] from {} into {}: {} shards per CF, {jobs} jobs",
        src_path.display(), out_dir.display(), shards);
    let progress = Progress {
        records: AtomicU64::new(0),
        skipped: AtomicU64::new(0),
        done_jobs: AtomicUsize::new(0),
    };
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let results: Mutex<Vec<Option<SegmentStats>>> = Mutex::new(vec![None; tasks.len()]);
    let start = Instant::now();
    std::thread::scope(|scope| -> Result<(), Error> {
        scope.spawn(|| {
            let mut last = 0u64;
            let mut ticks = 0u64;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                ticks += 1;
                if !ticks.is_multiple_of(30) {
                    continue;
                }
                let now = progress.records.load(Ordering::Relaxed);
                info!(target: "history_v3",
                    "{} records ({:.0}/s), {}/{} segments done, {:.0}s",
                    now, (now - last) as f64 / 30.0,
                    progress.done_jobs.load(Ordering::Relaxed), tasks.len(),
                    start.elapsed().as_secs_f64());
                last = now;
            }
        });
        let workers: Vec<_> = (0..jobs.max(1))
            .map(|_| {
                scope.spawn(|| -> Result<(), Error> {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(kind, shard)) = tasks.get(i) else {
                            return Ok(());
                        };
                        let path = out_dir
                            .join(&dir)
                            .join(format!("{}-{shard:02}.seg", kind.name()));
                        let stats = build_shard(
                            &src, kind, shard, shard_bits, head, &path, readahead, &progress,
                        )?;
                        info!(target: "history_v3",
                            "{} shard {shard}: {} records, {} keys, {} pages, {} fence levels, {:.1} GiB",
                            kind.name(), stats.records, stats.keys, stats.data_pages,
                            stats.fence_levels, stats.bytes as f64 / (1u64 << 30) as f64);
                        results.lock().unwrap()[i] = Some(stats);
                        progress.done_jobs.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        let mut result = Ok(());
        for w in workers {
            let r = w
                .join()
                .unwrap_or_else(|_| Err(Error::UnSupported("v3 build worker panicked".into())));
            if result.is_ok() {
                result = r;
            }
        }
        stop.store(true, Ordering::Relaxed);
        result
    })?;
    let results = results.into_inner().unwrap();
    let files = |kind: SegKind| -> Vec<ManifestFile> {
        tasks
            .iter()
            .zip(&results)
            .filter(|((k, _), _)| *k == kind)
            .map(|(_, stats)| {
                let stats = stats.as_ref().expect("every task finished");
                ManifestFile {
                    file: stats
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    bytes: stats.bytes,
                    records: stats.records,
                }
            })
            .collect()
    };
    let manifest = Manifest {
        schema: 3,
        sealed_upto: head,
        shard_bits,
        epochs: vec![ManifestEpoch {
            start: 0,
            end: head,
            dir,
            account: files(SegKind::Account),
            storage: files(SegKind::Storage),
        }],
    };
    // The segments' directory entries must be durable before the manifest
    // that names them.
    std::fs::File::open(out_dir.join(&manifest.epochs[0].dir))
        .and_then(|d| d.sync_all())
        .map_err(io_err)?;
    manifest.write(out_dir).map_err(io_err)?;
    let skipped = progress.skipped.load(Ordering::Relaxed);
    if skipped > 0 {
        warn!(target: "history_v3", "skipped {skipped} versions above the head {head}");
    }
    info!(target: "history_v3", "built {} records in {:.0}s",
        progress.records.load(Ordering::Relaxed), start.elapsed().as_secs_f64());
    Ok(manifest)
}

/// Outcome of [`verify_history_v3`].
#[derive(Debug, Default)]
pub struct VerifyReport {
    pub keys: u64,
    pub reads: u64,
    pub absent_keys: u64,
    pub mismatches: u64,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

/// Values equal under each kind's encoding (v2 storage is fixed 32 bytes).
fn same_value(kind: SegKind, v2: &[u8], v3: &[u8]) -> bool {
    match kind {
        SegKind::Storage => U256::from_be_slice(v2) == U256::from_be_slice(v3),
        SegKind::Account => v2 == v3,
    }
}

/// Compare sealed history at `history_dir` with the v2 archive it was built
/// from: for `samples` random keys per CF, read every version boundary and
/// random heights both ways, including the next-version bound the state
/// cache relies on. Also checks random absent keys.
pub fn verify_history_v3(
    src_path: &Path,
    history_dir: &Path,
    samples: u64,
    seed: u64,
) -> Result<VerifyReport, Error> {
    const MAX_VERSIONS: usize = 200_000;
    let src = open_v2_read_only(src_path)?;
    let store = HistoryStore::open(history_dir).map_err(io_err)?;
    let head = store.sealed_upto;
    let mut rng = Rng(seed | 1);
    let mut report = VerifyReport::default();
    let report_mismatch = |report: &mut VerifyReport, msg: String| {
        report.mismatches += 1;
        if report.mismatches <= 20 {
            warn!(target: "history_v3", "MISMATCH {msg}");
        }
    };
    for kind in [SegKind::Account, SegKind::Storage] {
        let k = kind.key_len();
        let cf = src.cf_handle(cf_name(kind)).unwrap();
        let mut ro = ReadOptions::default();
        ro.set_total_order_seek(true);
        let mut it = src.raw_iterator_cf_opt(cf, ro);
        for _ in 0..samples {
            let mut probe = vec![0u8; k];
            for b in probe.iter_mut() {
                *b = rng.next() as u8;
            }
            // A random key is (almost surely) absent.
            let epoch = store.epoch(rng.below(head + 1)).unwrap();
            if let Some(v) = epoch.lookup(kind, &probe, head).version {
                let mut seek = probe.clone();
                seek.extend_from_slice(&[0u8; 32]);
                it.seek(&seek);
                let exists = it.key().is_some_and(|key| key[..k] == probe[..]);
                if !exists {
                    report_mismatch(
                        &mut report,
                        format!("{} absent key found at {}", kind.name(), v.block),
                    );
                }
            } else {
                report.absent_keys += 1;
            }
            // Land on a real key and collect its versions, newest first.
            let mut seek = probe.clone();
            seek.extend_from_slice(&[0u8; 32]);
            it.seek(&seek);
            if it.key().is_none() {
                it.seek_to_first();
            }
            let Some(first) = it.key() else { break };
            let key = first[..k].to_vec();
            let mut versions: Vec<(u64, Vec<u8>)> = Vec::new();
            while let (Some(raw), Some(value)) = (it.key(), it.value()) {
                if raw[..k] != key[..] || versions.len() == MAX_VERSIONS {
                    break;
                }
                let block = tail_block(&raw[k..]);
                if block <= head {
                    versions.push((block, value.to_vec()));
                }
                it.next();
            }
            it.status().map_err(Error::RocksDB)?;
            if versions.is_empty() {
                continue;
            }
            report.keys += 1;
            let complete = versions.len() < MAX_VERSIONS;
            versions.reverse(); // oldest first
            let mut heights: Vec<u64> = Vec::new();
            for _ in 0..8 {
                let (b, _) = versions[rng.below(versions.len() as u64) as usize];
                heights.extend([b, b.saturating_sub(1), (b + 1).min(head)]);
                heights.push(rng.below(head + 1));
            }
            heights.extend([0, head, versions[0].0, versions[versions.len() - 1].0]);
            for h in heights {
                let i = versions.partition_point(|(b, _)| *b <= h);
                let want = (i > 0).then(|| &versions[i - 1]);
                let want_next = versions.get(i).map(|(b, _)| *b);
                let epoch = store.epoch(h).unwrap();
                let read = epoch.lookup(kind, &key, h);
                report.reads += 1;
                let key_hex = H256::from_slice(&key[..32]);
                if complete && read.next != want_next {
                    report_mismatch(
                        &mut report,
                        format!(
                            "{} {key_hex} h={h}: next v2 {want_next:?} v3 {:?}",
                            kind.name(),
                            read.next
                        ),
                    );
                }
                match (want, read.version) {
                    (None, None) => {}
                    (Some((b, v)), Some(g)) => {
                        if g.block != *b || !same_value(kind, v, g.value) {
                            report_mismatch(
                                &mut report,
                                format!(
                                    "{} {key_hex} h={h}: v2 ({b}, {v:02x?}) v3 ({}, {:02x?})",
                                    kind.name(),
                                    g.block,
                                    g.value
                                ),
                            );
                        }
                    }
                    (w, g) => report_mismatch(
                        &mut report,
                        format!(
                            "{} {key_hex} h={h}: v2 {:?} v3 {:?}",
                            kind.name(),
                            w.map(|(b, _)| *b),
                            g.map(|g| g.block)
                        ),
                    ),
                }
            }
        }
        info!(target: "history_v3", "verified {}: {report:?}", kind.name());
    }
    Ok(report)
}
