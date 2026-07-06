use crate::utils::{
    s3_get_block_info_and_diff_by_number, s3_get_block_info_and_diff_by_number_for_genesis,
};
use anyhow::Result;
use aws_sdk_s3::Client;
use clap::Parser;
use futures::{stream, StreamExt};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_storage::{
    encode_account_key, encode_slim_account, encode_storage_key, ArchiveRocksDBStorage,
    MDBXArchiveOptions, MDBXArchiveStorage, MDBXArchiveWriteBatch, MDBXSyncMode, StateDBWrite,
    StorageKind,
};
use leafage_evm_types::{BlockInfo, NewCode, H256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{info, warn};

/// Maximum retry attempts for failed blocks
const MAX_RETRIES: u32 = 3;

/// Delay between retries
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Delay for RocksDB to settle after closing (background threads to terminate)
const ROCKSDB_SETTLE_DELAY: Duration = Duration::from_secs(10);

/// MDBX sync mode for durability vs performance trade-off (MDBX only)
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum MdbxSyncMode {
    /// Default robust and durable sync mode. Safest but slowest.
    Durable,
    /// Don't sync the meta-page after commit. ~2x write performance.
    /// Database integrity preserved, but system crash may undo last committed transaction.
    NoMetaSync,
    /// Asynchronous mmap-flushes. ~10x write performance.
    /// Keeps previous steady commits, safer than UtterlyNoSync.
    SafeNoSync,
    /// No sync at all. Maximum write performance but least safe.
    /// Use only when data can be re-fetched (e.g., archive init from S3).
    #[default]
    UtterlyNoSync,
}

impl From<MdbxSyncMode> for MDBXSyncMode {
    fn from(mode: MdbxSyncMode) -> Self {
        match mode {
            MdbxSyncMode::Durable => MDBXSyncMode::Durable,
            MdbxSyncMode::NoMetaSync => MDBXSyncMode::NoMetaSync,
            MdbxSyncMode::SafeNoSync => MDBXSyncMode::SafeNoSync,
            MdbxSyncMode::UtterlyNoSync => MDBXSyncMode::UtterlyNoSync,
        }
    }
}

/// `leafage-evm archive-init` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to the database
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// The RPC address for fetching block info
    #[arg(long, value_name = "URL")]
    rpc_addr: String,

    /// S3 bucket name for block diff
    #[arg(long)]
    s3_bucket: String,

    /// S3 outer bucket name
    #[arg(long)]
    s3_outer_bucket: String,

    /// S3 chain ID
    #[arg(long)]
    s3_chain_id: String,

    /// S3 version (optional)
    #[arg(long, default_value = "")]
    s3_version: String,

    /// End block number (inclusive)
    #[arg(long)]
    end_block: u64,

    /// Storage type (rocksdb or mdbx)
    #[arg(long, default_value = "rocksdb")]
    db_type: StorageKind,

    /// Database cache size in MB (RocksDB only)
    #[arg(long, default_value = "2048")]
    db_cache: usize,

    /// MDBX initial database size in GB (MDBX only)
    #[arg(long, default_value = "1")]
    mdbx_initial_size_gb: usize,

    /// MDBX maximum database size in GB (MDBX only)
    #[arg(long, default_value = "1024")]
    mdbx_max_size_gb: usize,

    /// MDBX sync mode for durability vs performance trade-off (MDBX only).
    /// - durable: Safest, slowest
    /// - no-meta-sync: ~2x faster, system crash may lose last txn
    /// - safe-no-sync: ~10x faster, async flush
    /// - utterly-no-sync: Fastest, no sync (default for archive-init)
    #[arg(long, value_enum, default_value = "utterly-no-sync")]
    mdbx_sync_mode: MdbxSyncMode,

    /// Max concurrent fetch tasks. Also used as the bounded channel capacity
    /// between fetchers and the database writer.
    #[arg(long, default_value = "256")]
    max_tasks: usize,

    /// Checkpoint interval for committing blocks to database
    #[arg(long, default_value = "1024")]
    checkpoint_interval: u64,

    /// Use ZSTD-with-dict compression at deep levels for the three large
    /// archive CFs (RocksDB only). Default: false (uniform LZ4). See standalone
    /// command `--archive-zstd-compression` for full trade-off notes. Applies
    /// both during bulk-load ingest and the post-ingest compact() reopen.
    #[arg(long, default_value = "false")]
    archive_zstd_compression: bool,

    /// Write account/storage keys with the inverted (descending) block-height
    /// encoding. Default: false (legacy ascending).
    ///
    /// Must match the `--inverted-block-encoding` flag the serving
    /// `standalone --archive` node runs with — the two layouts are mutually
    /// unreadable.
    #[arg(long, default_value = "false")]
    inverted_block_encoding: bool,
}

/// Data fetched and pre-encoded for a single block, to be written by the
/// checkpoint worker.
///
/// All key/value byte encoding (account/storage) happens inside the fetcher
/// task so the writer thread only does sort + cursor put + commit.
struct EncodedBlockData {
    block_num: u64,
    block_hash: H256,
    block_info: BlockInfo,
    /// Pre-encoded account writes: `(address(32) || block_num(32), Some(rlp(SlimAccount)))`
    /// or `(.., None)` for deletions.
    accounts: Vec<([u8; 64], Option<Vec<u8>>)>,
    /// Pre-encoded storage writes: `(address(32) || key(32) || block_num(32), value_be_bytes(32))`.
    storage: Vec<([u8; 96], [u8; 32])>,
    /// New code blobs to write under `HashToCode`.
    codes: Vec<NewCode>,
}

/// Unified archive storage abstraction
#[derive(Debug)]
enum ArchiveStorage {
    RocksDB(Arc<ArchiveRocksDBStorage>),
    MDBX(Arc<MDBXArchiveStorage>),
}

/// Unified write batch abstraction
type RocksDBArchiveWriteBatch = <Arc<ArchiveRocksDBStorage> as StateDBWrite>::DBWriteBatch;

enum ArchiveWriteBatch {
    RocksDB(RocksDBArchiveWriteBatch),
    MDBX(MDBXArchiveWriteBatch),
}

impl ArchiveStorage {
    fn read_latest_block_hash(&self) -> Result<H256, anyhow::Error> {
        match self {
            ArchiveStorage::RocksDB(db) => Ok(db.read_latest_block_hash()?),
            ArchiveStorage::MDBX(db) => Ok(db.read_latest_block_hash()?),
        }
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<BlockInfo>, anyhow::Error> {
        match self {
            ArchiveStorage::RocksDB(db) => Ok(db.read_block_info(block_hash)?),
            ArchiveStorage::MDBX(db) => Ok(db.read_block_info(block_hash)?),
        }
    }

    fn prepare_write_batch(&self) -> Result<ArchiveWriteBatch, anyhow::Error> {
        match self {
            ArchiveStorage::RocksDB(db) => {
                Ok(ArchiveWriteBatch::RocksDB(db.prepare_write_batch()?))
            }
            ArchiveStorage::MDBX(db) => Ok(ArchiveWriteBatch::MDBX(db.prepare_write_batch()?)),
        }
    }

    fn write_latest_block_hash(
        &self,
        batch: &mut ArchiveWriteBatch,
        block_hash: H256,
    ) -> Result<(), anyhow::Error> {
        match (self, batch) {
            (ArchiveStorage::RocksDB(db), ArchiveWriteBatch::RocksDB(b)) => {
                Ok(db.write_latest_block_hash(b, block_hash)?)
            }
            (ArchiveStorage::MDBX(db), ArchiveWriteBatch::MDBX(b)) => {
                Ok(db.write_latest_block_hash(b, block_hash)?)
            }
            _ => Err(anyhow::anyhow!("Batch type mismatch")),
        }
    }

    fn write_block_hash(
        &self,
        batch: &mut ArchiveWriteBatch,
        block_num: u64,
        block_hash: H256,
    ) -> Result<(), anyhow::Error> {
        match (self, batch) {
            (ArchiveStorage::RocksDB(db), ArchiveWriteBatch::RocksDB(b)) => {
                Ok(db.write_block_hash(b, block_num, block_hash)?)
            }
            (ArchiveStorage::MDBX(db), ArchiveWriteBatch::MDBX(b)) => {
                Ok(db.write_block_hash(b, block_num, block_hash)?)
            }
            _ => Err(anyhow::anyhow!("Batch type mismatch")),
        }
    }

    fn write_block_info(
        &self,
        batch: &mut ArchiveWriteBatch,
        block_info: BlockInfo,
    ) -> Result<(), anyhow::Error> {
        match (self, batch) {
            (ArchiveStorage::RocksDB(db), ArchiveWriteBatch::RocksDB(b)) => {
                Ok(db.write_block_info(b, block_info)?)
            }
            (ArchiveStorage::MDBX(db), ArchiveWriteBatch::MDBX(b)) => {
                Ok(db.write_block_info(b, block_info)?)
            }
            _ => Err(anyhow::anyhow!("Batch type mismatch")),
        }
    }

    /// Append pre-encoded account entries to the deferred cache (no encoding,
    /// no per-entry trait dispatch on the writer thread).
    fn extend_account_writes(
        &self,
        batch: &mut ArchiveWriteBatch,
        items: Vec<([u8; 64], Option<Vec<u8>>)>,
    ) -> Result<(), anyhow::Error> {
        match batch {
            ArchiveWriteBatch::RocksDB(b) => b.extend_account_writes(items),
            ArchiveWriteBatch::MDBX(b) => b.extend_account_writes(items),
        }
        Ok(())
    }

    /// Append pre-encoded storage entries to the deferred cache.
    fn extend_storage_writes(
        &self,
        batch: &mut ArchiveWriteBatch,
        items: Vec<([u8; 96], [u8; 32])>,
    ) -> Result<(), anyhow::Error> {
        match batch {
            ArchiveWriteBatch::RocksDB(b) => b.extend_storage_writes(items),
            ArchiveWriteBatch::MDBX(b) => b.extend_storage_writes(items),
        }
        Ok(())
    }

    fn write_code(
        &self,
        batch: &mut ArchiveWriteBatch,
        code_hash: H256,
        code: leafage_evm_types::Bytes,
    ) -> Result<(), anyhow::Error> {
        match (self, batch) {
            (ArchiveStorage::RocksDB(db), ArchiveWriteBatch::RocksDB(b)) => {
                Ok(db.write_code(b, code_hash, code)?)
            }
            (ArchiveStorage::MDBX(db), ArchiveWriteBatch::MDBX(b)) => {
                Ok(db.write_code(b, code_hash, code)?)
            }
            _ => Err(anyhow::anyhow!("Batch type mismatch")),
        }
    }

    /// Commit the batch and ensure durability before returning. For RocksDB
    /// this fsyncs the WAL segment for this batch (so a crash resumes from
    /// here, not earlier). For MDBX this commits, then explicitly syncs the
    /// environment (covers UtterlyNoSync mode where commit alone doesn't
    /// flush to disk).
    fn commit_sync(&self, batch: ArchiveWriteBatch) -> Result<(), anyhow::Error> {
        match (self, batch) {
            (ArchiveStorage::RocksDB(db), ArchiveWriteBatch::RocksDB(mut b)) => {
                b.set_sync(true);
                Ok(db.commit(b)?)
            }
            (ArchiveStorage::MDBX(db), ArchiveWriteBatch::MDBX(b)) => {
                db.commit(b)?;
                db.sync(true)?;
                Ok(())
            }
            _ => Err(anyhow::anyhow!("Batch type mismatch")),
        }
    }

    /// Flush database to disk.
    /// For RocksDB: uses WAL flush.
    /// For MDBX: uses env.sync(force=true) to ensure durability, especially important
    /// when using UtterlyNoSync mode.
    fn flush(&self) -> Result<(), anyhow::Error> {
        match self {
            ArchiveStorage::RocksDB(db) => Ok(db.flush()?),
            ArchiveStorage::MDBX(db) => {
                db.sync(true)?;
                Ok(())
            }
        }
    }
}

// ===== RocksDB SST ingest pipeline =====
//
// The default RocksDB write path goes block_data → WriteBatch → memtable →
// (optional) WAL fsync at checkpoint. For archive bulk-load that path's
// per-checkpoint cost is dominated by memtable insert (millions of skiplist
// inserts) and WAL append (~1 GB sequential write per 1024-block batch),
// neither of which is necessary for a one-shot, replayable ingest.
//
// This pipeline replaces the path with a fan-out:
//
//   fetcher → batch_accumulator → ingest_dispatcher (N workers) → watermark_advancer
//
// `batch_accumulator` reorders out-of-order fetcher output by block_num,
// accumulates `checkpoint_interval` blocks per batch, and emits one
// `BatchPayload` per batch.
//
// `ingest_dispatcher` runs up to `INGEST_WORKER_COUNT` workers in parallel.
// Each worker sorts its batch's pre-encoded account/storage writes, builds
// per-CF SST files via `SstFileWriter`, and ingests them via
// `ingest_external_file_cf` (which bypasses memtable and WAL entirely; the
// files are atomically registered in the MANIFEST and physically moved into
// the DB's data directory).
//
// `watermark_advancer` collects worker completions, drains them in `batch_seq`
// order (so completions can arrive in any order), commits each batch's small
// CFs (block_info / block_num→hash / code) via the regular WriteBatch path,
// and only on the last batch of each in-order drain marks the WriteBatch
// `set_sync(true)` and writes `latest_block_hash`. The single WAL fsync per
// drain is the only durability fence; on crash, resume reads
// `latest_block_hash`, recomputes start_block = N+1, and re-ingests blocks
// > N. Re-ingest produces SSTs whose keys collide with previously-ingested
// SSTs, but RocksDB assigns a strictly later global seqno to the new files,
// so `read_merge` correctly returns the new value; the final manual
// `compact()` collapses the duplicates.
//
// Correctness invariant: account_key = `address(32) || block_num(32 BE)` and
// storage_key = `address(32) || slot(32) || block_num(32 BE)` are unique
// within and across batches (block_num appears in the key, and within one
// block the upstream tracer guarantees `deleted_accounts ∩ new_accounts = ∅`).
// `dedup_keep_last_sorted` is applied defensively after sort to make the
// SstFileWriter's strictly-increasing-key contract robust against any
// unexpected duplicates.

const INGEST_WORKER_COUNT: usize = 4;
const INGEST_TMP_SUBDIR: &str = ".ingest_tmp";

/// One checkpoint's worth of writes, handed off from the accumulator to a
/// worker. Workers consume `account_writes` / `storage_writes`, build SSTs,
/// and ingest them; `small_writes` is passed through unchanged for the
/// advancer to commit.
struct BatchPayload {
    batch_seq: u64,
    last_block_num: u64,
    last_block_hash: H256,
    blocks_in_batch: u64,
    account_writes: Vec<([u8; 64], Vec<u8>)>,
    storage_writes: Vec<([u8; 96], [u8; 32])>,
    small_writes: RocksDBArchiveWriteBatch,
}

/// Worker → advancer message. Carries everything the advancer needs to finish
/// the batch's commit (small writes + the resume-pointer info), plus per-stage
/// timing collected by the worker.
struct BatchCompletion {
    batch_seq: u64,
    last_block_num: u64,
    last_block_hash: H256,
    blocks_in_batch: u64,
    small_writes: RocksDBArchiveWriteBatch,
    timings: BatchTimings,
}

/// Per-batch stage timing + size accounting. Worker fills in `sort_dedup_us`,
/// `write_sst_us`, and `ingest_us`; advancer fills in `commit_us`. Used both
/// for per-batch info logging and end-of-run aggregate summary.
#[derive(Default, Clone, Copy)]
struct BatchTimings {
    /// `sort_by` + `dedup_keep_last_sorted` for account + storage combined.
    sort_dedup_us: u64,
    /// `write_account_sst` + `write_storage_sst` combined (SstFileWriter
    /// streaming write to disk, includes LZ4 encoding).
    write_sst_us: u64,
    /// `ingest_account_ssts` + `ingest_storage_ssts` combined (RocksDB
    /// MANIFEST update + file rename via `move_files=true`; serialized on the
    /// DB internal mutex).
    ingest_us: u64,
    /// Advancer's `StateDBWrite::commit` for the small WriteBatch
    /// (block_info + block_hash + code; the last batch in each drain also
    /// includes `latest_block_hash` and pays the WAL fsync).
    commit_us: u64,
    account_count: u64,
    storage_count: u64,
    account_bytes: u64,
    storage_bytes: u64,
}

impl BatchTimings {
    fn add(&mut self, other: &BatchTimings) {
        self.sort_dedup_us += other.sort_dedup_us;
        self.write_sst_us += other.write_sst_us;
        self.ingest_us += other.ingest_us;
        self.commit_us += other.commit_us;
        self.account_count += other.account_count;
        self.storage_count += other.storage_count;
        self.account_bytes += other.account_bytes;
        self.storage_bytes += other.storage_bytes;
    }
}

/// Sort-then-keep-last on a sorted Vec of (key, value) pairs. Equal-key runs
/// collapse to the rightmost entry. Defensive against any upstream invariant
/// drift that would otherwise violate `SstFileWriter`'s strictly-increasing
/// key contract.
fn dedup_keep_last_sorted<K: Eq, V>(v: &mut Vec<(K, V)>) {
    if v.len() <= 1 {
        return;
    }
    let mut w: usize = 0;
    let mut r: usize = 0;
    while r < v.len() {
        let mut e = r;
        while e + 1 < v.len() && v[e + 1].0 == v[r].0 {
            e += 1;
        }
        if w != e {
            v.swap(w, e);
        }
        w += 1;
        r = e + 1;
    }
    v.truncate(w);
}

/// Drain `EncodedBlockData` from the fetcher channel, reorder by block_num,
/// and emit one `BatchPayload` per `checkpoint_interval` blocks. The final
/// partial batch (blocks past the last checkpoint boundary) is emitted on
/// drain. Returns the highest contiguously-absorbed block number.
fn spawn_rocksdb_batch_accumulator(
    mut block_rx: mpsc::Receiver<EncodedBlockData>,
    batch_tx: mpsc::Sender<BatchPayload>,
    db: Arc<ArchiveRocksDBStorage>,
    start_block: u64,
    checkpoint_interval: u64,
) -> tokio::task::JoinHandle<u64> {
    tokio::task::spawn_blocking(move || {
        let mut pending: BTreeMap<u64, EncodedBlockData> = BTreeMap::new();
        let mut max_contiguous: Option<u64> = if start_block == 0 {
            None
        } else {
            Some(start_block - 1)
        };
        let mut last_checkpoint_num = start_block.saturating_sub(1) / checkpoint_interval;
        let mut batch_seq: u64 = 0;

        let mut acc_writes: Vec<([u8; 64], Vec<u8>)> = Vec::new();
        let mut sto_writes: Vec<([u8; 96], [u8; 32])> = Vec::new();
        let mut small = db
            .prepare_write_batch()
            .expect("Failed to prepare initial small batch");
        let mut blocks_in_batch: u64 = 0;
        let mut last_block_hash_in_batch: Option<H256> = None;
        let mut last_block_num_in_batch: u64 = 0;

        while let Some(block) = block_rx.blocking_recv() {
            let bn = block.block_num;
            if max_contiguous.is_some_and(|mc| bn <= mc) {
                warn!(target: "archive_init",
                    "Duplicate block {} received after it was already absorbed; ignoring", bn);
                continue;
            }
            if pending.insert(bn, block).is_some() {
                warn!(target: "archive_init",
                    "Duplicate pending block {} received; replacing previous", bn);
            }

            let mut next = max_contiguous.map(|mc| mc + 1).unwrap_or(start_block);
            while let Some(b) = pending.remove(&next) {
                let bh = b.block_hash;
                let bnum = b.block_num;

                StateDBWrite::write_block_hash(
                    &db,
                    &mut small,
                    b.block_info.header.number,
                    bh,
                )
                .expect("write_block_hash");
                StateDBWrite::write_block_info(&db, &mut small, b.block_info)
                    .expect("write_block_info");
                acc_writes.extend(
                    b.accounts
                        .into_iter()
                        .map(|(k, v)| (k, v.unwrap_or_default())),
                );
                sto_writes.extend(b.storage);
                for code in b.codes {
                    StateDBWrite::write_code(&db, &mut small, code.code_hash, code.code)
                        .expect("write_code");
                }

                max_contiguous = Some(bnum);
                blocks_in_batch += 1;
                last_block_hash_in_batch = Some(bh);
                last_block_num_in_batch = bnum;

                let chk = bnum / checkpoint_interval;
                if chk > last_checkpoint_num {
                    let payload = BatchPayload {
                        batch_seq,
                        last_block_num: last_block_num_in_batch,
                        last_block_hash: last_block_hash_in_batch.expect("hash set"),
                        blocks_in_batch,
                        account_writes: std::mem::take(&mut acc_writes),
                        storage_writes: std::mem::take(&mut sto_writes),
                        small_writes: std::mem::replace(
                            &mut small,
                            db.prepare_write_batch().expect("prepare next small"),
                        ),
                    };
                    if batch_tx.blocking_send(payload).is_err() {
                        // Worker pool dropped the receiver: pipeline aborted.
                        return max_contiguous.unwrap_or(start_block.saturating_sub(1));
                    }
                    batch_seq += 1;
                    last_checkpoint_num = chk;
                    blocks_in_batch = 0;
                    last_block_hash_in_batch = None;
                }

                next += 1;
            }
        }

        if blocks_in_batch > 0 {
            let payload = BatchPayload {
                batch_seq,
                last_block_num: last_block_num_in_batch,
                last_block_hash: last_block_hash_in_batch.expect("hash set"),
                blocks_in_batch,
                account_writes: std::mem::take(&mut acc_writes),
                storage_writes: std::mem::take(&mut sto_writes),
                small_writes: small,
            };
            let _ = batch_tx.blocking_send(payload);
        }

        max_contiguous.unwrap_or(start_block.saturating_sub(1))
    })
}

/// CPU + disk work: sort the deferred writes, build SST files, ingest them.
/// Runs on tokio's blocking pool; ingest serializes on RocksDB's internal
/// mutex but each call is fast (no memtable flush since these CFs aren't
/// written through memtable).
fn ingest_worker_process(
    db: Arc<ArchiveRocksDBStorage>,
    tmp_dir: PathBuf,
    mut payload: BatchPayload,
) -> Result<BatchCompletion> {
    let mut timings = BatchTimings::default();

    // Pre-stage byte/count accounting before any consume. Each account
    // tuple = 64-byte key + Vec<u8> value (RLP slim account, ~50–100B).
    // Each storage tuple = 96-byte key + 32-byte value.
    timings.account_count = payload.account_writes.len() as u64;
    timings.storage_count = payload.storage_writes.len() as u64;
    timings.account_bytes = payload
        .account_writes
        .iter()
        .map(|(k, v)| (k.len() + v.len()) as u64)
        .sum();
    timings.storage_bytes = payload
        .storage_writes
        .iter()
        .map(|(k, v)| (k.len() + v.len()) as u64)
        .sum();

    // STABLE sort: when the same key appears more than once within a batch,
    // the last write must win (matches the legacy WriteBatch path's semantics
    // at archive/mod.rs:1573 and 1598). An unstable sort would leave equal
    // keys in arbitrary order, so `dedup_keep_last_sorted` could keep an
    // earlier write instead of the latest one. The runtime cost difference vs
    // unstable is small (sort_by uses Timsort, ~1.5–2× of pdqsort on random
    // data of this size); correctness wins. The underlying invariant
    // (BlockStorageDiff produces unique keys per block) is upstream and not
    // statically enforced, so we don't rely on it.
    let sort_t = Instant::now();
    payload.account_writes.sort_by(|a, b| a.0.cmp(&b.0));
    dedup_keep_last_sorted(&mut payload.account_writes);
    payload.storage_writes.sort_by(|a, b| a.0.cmp(&b.0));
    dedup_keep_last_sorted(&mut payload.storage_writes);
    timings.sort_dedup_us = sort_t.elapsed().as_micros() as u64;

    let acc_path = tmp_dir.join(format!("acc_{:020}.sst", payload.batch_seq));
    let sto_path = tmp_dir.join(format!("sto_{:020}.sst", payload.batch_seq));

    let write_t = Instant::now();
    if !payload.account_writes.is_empty() {
        db.write_account_sst(&acc_path, &payload.account_writes)
            .map_err(|e| anyhow::anyhow!("write_account_sst failed: {e}"))?;
    }
    if !payload.storage_writes.is_empty() {
        db.write_storage_sst(&sto_path, &payload.storage_writes)
            .map_err(|e| anyhow::anyhow!("write_storage_sst failed: {e}"))?;
    }
    timings.write_sst_us = write_t.elapsed().as_micros() as u64;

    let ingest_t = Instant::now();
    if !payload.account_writes.is_empty() {
        db.ingest_account_ssts(vec![acc_path])
            .map_err(|e| anyhow::anyhow!("ingest_account_ssts failed: {e}"))?;
    }
    if !payload.storage_writes.is_empty() {
        db.ingest_storage_ssts(vec![sto_path])
            .map_err(|e| anyhow::anyhow!("ingest_storage_ssts failed: {e}"))?;
    }
    timings.ingest_us = ingest_t.elapsed().as_micros() as u64;

    Ok(BatchCompletion {
        batch_seq: payload.batch_seq,
        last_block_num: payload.last_block_num,
        last_block_hash: payload.last_block_hash,
        blocks_in_batch: payload.blocks_in_batch,
        small_writes: payload.small_writes,
        timings,
    })
}

/// Bounded worker pool: at most `worker_count` ingest workers in flight.
/// Workers complete out of order; the advancer is responsible for re-ordering
/// by `batch_seq`. Worker errors abort the pipeline by dropping the
/// completion channel, propagating the failure through the rest of the
/// pipeline.
fn spawn_rocksdb_ingest_dispatcher(
    mut batch_rx: mpsc::Receiver<BatchPayload>,
    completion_tx: mpsc::Sender<BatchCompletion>,
    db: Arc<ArchiveRocksDBStorage>,
    tmp_dir: PathBuf,
    worker_count: usize,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let mut tasks: tokio::task::JoinSet<Result<BatchCompletion>> =
            tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;
                done = tasks.join_next(), if tasks.len() >= worker_count => {
                    let res = done.expect("len >= worker_count > 0");
                    let comp = res
                        .map_err(|e| anyhow::anyhow!("ingest worker join failed: {e}"))??;
                    if completion_tx.send(comp).await.is_err() {
                        return Err(anyhow::anyhow!(
                            "watermark advancer dropped completion channel"
                        ));
                    }
                }
                batch = batch_rx.recv() => {
                    match batch {
                        Some(b) => {
                            let db_clone = db.clone();
                            let tmp_clone = tmp_dir.clone();
                            tasks.spawn_blocking(move || {
                                ingest_worker_process(db_clone, tmp_clone, b)
                            });
                        }
                        None => break,
                    }
                }
            }
        }
        while let Some(res) = tasks.join_next().await {
            let comp = res
                .map_err(|e| anyhow::anyhow!("ingest worker join failed: {e}"))??;
            if completion_tx.send(comp).await.is_err() {
                return Err(anyhow::anyhow!(
                    "watermark advancer dropped completion channel"
                ));
            }
        }
        Ok(())
    })
}

/// Drain `BatchCompletion`s in `batch_seq` order. For each in-order drain,
/// commit every batch's small writes (block_info / hash / code); on the last
/// batch of the drain, also write `latest_block_hash` and mark the
/// WriteBatch `set_sync(true)` so the trailing WAL fsync covers all
/// preceding non-synced writes (WAL is sequential).
fn spawn_rocksdb_watermark_advancer(
    mut completion_rx: mpsc::Receiver<BatchCompletion>,
    db: Arc<ArchiveRocksDBStorage>,
    start_block: u64,
    overall_start: Instant,
    total_blocks: u64,
) -> tokio::task::JoinHandle<(u64, u64)> {
    tokio::task::spawn_blocking(move || {
        let mut pending: BTreeMap<u64, BatchCompletion> = BTreeMap::new();
        let mut next_seq: u64 = 0;
        let mut written_count: u64 = 0;
        let mut final_contiguous = start_block.saturating_sub(1);
        let mut last_progress_log = Instant::now();

        // Aggregate stage timings across all committed batches; emitted as a
        // summary line at end of run for tuning.
        let mut total_timings = BatchTimings::default();
        let mut total_batches: u64 = 0;

        while let Some(comp) = completion_rx.blocking_recv() {
            pending.insert(comp.batch_seq, comp);

            let mut drained: Vec<BatchCompletion> = Vec::new();
            while let Some(c) = pending.remove(&next_seq) {
                drained.push(c);
                next_seq += 1;
            }
            if drained.is_empty() {
                continue;
            }

            let last_idx = drained.len() - 1;
            for (i, mut comp) in drained.into_iter().enumerate() {
                let is_last = i == last_idx;
                if is_last {
                    StateDBWrite::write_latest_block_hash(
                        &db,
                        &mut comp.small_writes,
                        comp.last_block_hash,
                    )
                    .expect("write_latest_block_hash");
                    comp.small_writes.set_sync(true);
                    final_contiguous = comp.last_block_num;
                }

                let commit_t = Instant::now();
                StateDBWrite::commit(&db, comp.small_writes).expect("small batch commit");
                comp.timings.commit_us = commit_t.elapsed().as_micros() as u64;

                written_count += comp.blocks_in_batch;
                total_timings.add(&comp.timings);
                total_batches += 1;

                let total_us = comp.timings.sort_dedup_us
                    + comp.timings.write_sst_us
                    + comp.timings.ingest_us
                    + comp.timings.commit_us;
                info!(target: "archive_init",
                    "batch_seq={} last_block={} accs={} stos={} acc={}MB sto={}MB | sort={}ms write_sst={}ms ingest={}ms commit{}={}ms total={}ms",
                    comp.batch_seq,
                    comp.last_block_num,
                    comp.timings.account_count,
                    comp.timings.storage_count,
                    comp.timings.account_bytes / (1024 * 1024),
                    comp.timings.storage_bytes / (1024 * 1024),
                    comp.timings.sort_dedup_us / 1000,
                    comp.timings.write_sst_us / 1000,
                    comp.timings.ingest_us / 1000,
                    if is_last { "(sync)" } else { "" },
                    comp.timings.commit_us / 1000,
                    total_us / 1000,
                );
            }

            if last_progress_log.elapsed() >= Duration::from_secs(1) {
                let elapsed = overall_start.elapsed().as_secs_f64();
                let bps = if elapsed > 0.0 {
                    written_count as f64 / elapsed
                } else {
                    0.0
                };
                let remaining = total_blocks.saturating_sub(written_count);
                let eta = if bps > 0.0 {
                    (remaining as f64 / bps) as u64
                } else {
                    0
                };
                let pct = (written_count.min(total_blocks) * 100) / total_blocks.max(1);
                info!(target: "archive_init",
                    "Progress: {}% ({}/{}) | Pending: {} | Speed: {:.1} blocks/s | ETA: {}s",
                    pct, written_count, total_blocks, pending.len(), bps, eta);
                last_progress_log = Instant::now();
            }
        }

        if !pending.is_empty() {
            warn!(target: "archive_init",
                "{} batches arrived but never reached the in-order prefix; expected next_seq = {}",
                pending.len(), next_seq);
        }

        if total_batches > 0 {
            // Aggregate stage breakdown across the entire run. With N>1
            // workers the per-stage `*_us` totals are the *sum* across batches
            // (wall-clock through the pipeline is much shorter because
            // workers run in parallel). The numbers compare stages against
            // each other, not against wall time.
            let sd = total_timings.sort_dedup_us / 1000;
            let ws = total_timings.write_sst_us / 1000;
            let ig = total_timings.ingest_us / 1000;
            let cm = total_timings.commit_us / 1000;
            let sum = (sd + ws + ig + cm).max(1);
            info!(target: "archive_init",
                "Stage totals (summed across {} batches; multi-worker overlap not reflected): \
                 sort+dedup {}ms ({}%) | write_sst {}ms ({}%) | ingest {}ms ({}%) | small commit+fsync {}ms ({}%)",
                total_batches,
                sd, sd * 100 / sum,
                ws, ws * 100 / sum,
                ig, ig * 100 / sum,
                cm, cm * 100 / sum,
            );
            info!(target: "archive_init",
                "Volume totals: accounts {} ({}MB) | storage {} ({}MB)",
                total_timings.account_count,
                total_timings.account_bytes / (1024 * 1024),
                total_timings.storage_count,
                total_timings.storage_bytes / (1024 * 1024),
            );
        }

        (written_count, final_contiguous)
    })
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        info!(target: "archive_init", "Starting archive initialization");
        info!(target: "archive_init", "db_path: {:?}, rpc_addr: {}, end_block: {}, max_tasks: {}, db_type: {:?}, inverted_block_encoding: {}",
              self.db_path, self.rpc_addr, self.end_block, self.max_tasks, self.db_type, self.inverted_block_encoding);

        // Fix the versioned-key encoding before any key is encoded (fetchers
        // call encode_account_key / encode_storage_key off the writer thread).
        leafage_evm_storage::set_inverted_block_encoding(self.inverted_block_encoding);

        // Validate checkpoint_interval
        if self.checkpoint_interval == 0 {
            anyhow::bail!("checkpoint_interval must be greater than 0");
        }
        if self.max_tasks == 0 {
            anyhow::bail!("max_tasks must be greater than 0");
        }

        // Initialize S3 client
        let s3_config = aws_config::load_from_env().await;
        let s3_client = aws_sdk_s3::Client::new(&s3_config);

        // Initialize RPC client
        let rpc_client = HttpClientBuilder::default().build(&self.rpc_addr)?;

        // Open archive database based on db type
        let db = Arc::new(match self.db_type {
            StorageKind::Rocksdb => {
                info!(target: "archive_init",
                    "Opening RocksDB archive database (bulk-load mode) with cache_size: {}MB",
                    self.db_cache);
                // Bulk-load mode: WAL off, L0/pending-compaction throttles off.
                // Auto-compactions stay ON so RocksDB drains L0 → L1 in the
                // background; the throttles are off so a slow compaction never
                // back-pressures the writer. Safe because archive data is
                // replayable from S3 on crash, and we still run a final
                // manual compact() at the end to consolidate into deep levels.
                ArchiveStorage::RocksDB(Arc::new(ArchiveRocksDBStorage::open_for_bulk_load(
                    &self.db_path,
                    self.db_cache,
                    self.archive_zstd_compression,
                )))
            }
            StorageKind::MDBX => {
                // Validate MDBX configuration
                if self.mdbx_initial_size_gb > self.mdbx_max_size_gb {
                    anyhow::bail!(
                        "MDBX initial size ({}GB) cannot be greater than max size ({}GB)",
                        self.mdbx_initial_size_gb,
                        self.mdbx_max_size_gb
                    );
                }
                const GB: usize = 1024 * 1024 * 1024;
                let options = MDBXArchiveOptions {
                    initial_size: self.mdbx_initial_size_gb * GB,
                    max_size: self.mdbx_max_size_gb * GB,
                    sync_mode: self.mdbx_sync_mode.into(),
                    ..Default::default()
                };
                info!(target: "archive_init", "Opening MDBX archive database with initial_size: {}GB, max_size: {}GB, sync_mode: {:?}",
                      self.mdbx_initial_size_gb, self.mdbx_max_size_gb, self.mdbx_sync_mode);
                ArchiveStorage::MDBX(Arc::new(MDBXArchiveStorage::open_with_options(
                    &self.db_path,
                    options,
                )))
            }
        });

        // Record the block-height key encoding in the DB so the node
        // auto-detects it at startup (the flag is already marker-aligned by the
        // open above for a resumed DB). RocksDB only; MDBX is unaffected.
        if let ArchiveStorage::RocksDB(rdb) = db.as_ref() {
            rdb.write_encoding_marker(leafage_evm_storage::inverted_block_encoding())
                .map_err(|e| anyhow::anyhow!("failed to write encoding marker: {e}"))?;
        }

        // Determine start block (support resume)
        let start_block = self.get_start_block(&db)?;

        if start_block > self.end_block {
            info!(target: "archive_init", "Database already contains blocks up to {}, nothing to do", start_block - 1);
            return Ok(());
        }

        let total_blocks = self.end_block - start_block + 1;
        info!(target: "archive_init", "Initializing from block {} to block {} ({} blocks total)",
              start_block, self.end_block, total_blocks);

        let overall_start = Instant::now();

        // Capture variables for the fetcher async block
        let rpc_client = Some(rpc_client);
        let bucket = self.s3_bucket.clone();
        let outer_bucket = self.s3_outer_bucket.clone();
        let chain_id = self.s3_chain_id.clone();
        let version = self.s3_version.clone();
        let max_tasks = self.max_tasks;
        let blocks = stream::iter(start_block..=self.end_block);

        // Branch on backend: RocksDB uses the SST ingest pipeline, MDBX keeps
        // the existing single-threaded checkpoint worker (MDBX has neither
        // SstFileWriter nor a comparable bulk-load path).
        let (final_success, final_contiguous) = match &*db {
            ArchiveStorage::RocksDB(rocks) => {
                let rocks = rocks.clone();

                // Prepare temp dir for SST files. Same filesystem as the DB so
                // `IngestExternalFileOptions::set_move_files(true)` can rename
                // instead of copy. Wipe any leftover from a previous failed
                // run before starting (only ingest temp files live here).
                let tmp_dir = self.db_path.join(INGEST_TMP_SUBDIR);
                if tmp_dir.exists() {
                    std::fs::remove_dir_all(&tmp_dir)?;
                }
                std::fs::create_dir_all(&tmp_dir)?;

                let (block_tx, block_rx) =
                    mpsc::channel::<EncodedBlockData>(self.max_tasks);
                let (batch_tx, batch_rx) =
                    mpsc::channel::<BatchPayload>(INGEST_WORKER_COUNT + 1);
                let (completion_tx, completion_rx) =
                    mpsc::channel::<BatchCompletion>(INGEST_WORKER_COUNT + 1);

                let accumulator_handle = spawn_rocksdb_batch_accumulator(
                    block_rx,
                    batch_tx,
                    rocks.clone(),
                    start_block,
                    self.checkpoint_interval,
                );
                let dispatcher_handle = spawn_rocksdb_ingest_dispatcher(
                    batch_rx,
                    completion_tx,
                    rocks.clone(),
                    tmp_dir.clone(),
                    INGEST_WORKER_COUNT,
                );
                let advancer_handle = spawn_rocksdb_watermark_advancer(
                    completion_rx,
                    rocks.clone(),
                    start_block,
                    overall_start,
                    total_blocks,
                );

                blocks
                    .map(|block_num| {
                        let rpc = rpc_client.clone();
                        let s3 = s3_client.clone();
                        let bucket = bucket.clone();
                        let outer_bucket = outer_bucket.clone();
                        let chain_id = chain_id.clone();
                        let version = version.clone();
                        async move {
                            Self::fetch_block_with_retry(
                                rpc,
                                s3,
                                bucket,
                                outer_bucket,
                                chain_id,
                                version,
                                block_num,
                            )
                            .await
                        }
                    })
                    .buffer_unordered(max_tasks)
                    .for_each(|block_data| {
                        let tx = block_tx.clone();
                        async move {
                            tx.send(block_data)
                                .await
                                .expect("Batch accumulator channel closed");
                        }
                    })
                    .await;
                drop(block_tx);

                let _accumulator_high_water = accumulator_handle.await?;
                dispatcher_handle.await??;
                let stats = advancer_handle.await?;

                // Best-effort cleanup; orphan files will be wiped at next
                // archive-init startup anyway.
                let _ = std::fs::remove_dir_all(&tmp_dir);
                stats
            }
            ArchiveStorage::MDBX(_) => {
                let (tx, rx) = mpsc::channel::<EncodedBlockData>(self.max_tasks);
                let checkpoint_worker = Self::spawn_checkpoint_worker(
                    rx,
                    db.clone(),
                    start_block,
                    total_blocks,
                    overall_start,
                    self.checkpoint_interval,
                );

                blocks
                    .map(|block_num| {
                        let rpc = rpc_client.clone();
                        let s3 = s3_client.clone();
                        let bucket = bucket.clone();
                        let outer_bucket = outer_bucket.clone();
                        let chain_id = chain_id.clone();
                        let version = version.clone();
                        async move {
                            Self::fetch_block_with_retry(
                                rpc,
                                s3,
                                bucket,
                                outer_bucket,
                                chain_id,
                                version,
                                block_num,
                            )
                            .await
                        }
                    })
                    .buffer_unordered(max_tasks)
                    .for_each(|block_data| {
                        let tx = tx.clone();
                        async move {
                            tx.send(block_data)
                                .await
                                .expect("Checkpoint worker channel closed");
                        }
                    })
                    .await;
                drop(tx);

                checkpoint_worker.await?
            }
        };

        let total_time = overall_start.elapsed().as_secs_f64();
        let avg_speed = final_success as f64 / total_time;

        // Verify all blocks were processed
        if final_contiguous < self.end_block {
            panic!(
                "Final contiguous block {} is less than end_block {}",
                final_contiguous, self.end_block
            );
        }

        info!(target: "archive_init",
            "Archive initialization completed. Total: {} blocks in {:.1}s ({:.1} blocks/s)",
            final_success, total_time, avg_speed);

        // RocksDB-specific: compaction phase
        if matches!(self.db_type, StorageKind::Rocksdb) {
            // Close database to ensure all writes are persisted before compaction
            info!(target: "archive_init", "Closing database before compaction...");
            Arc::try_unwrap(db).expect("Database Arc has other references, cannot close safely");

            // Wait for RocksDB background threads to fully terminate
            sleep(ROCKSDB_SETTLE_DELAY).await;

            // Reopen database with auto compaction enabled for the compaction phase
            let compact_db = ArchiveRocksDBStorage::open(
                &self.db_path,
                self.db_cache,
                false,
                self.archive_zstd_compression,
            );
            info!(target: "archive_init", "Starting database compaction...");
            compact_db.compact()?;
            info!(target: "archive_init", "Database compaction completed.");
        }

        Ok(())
    }

    /// Spawn checkpoint worker that handles block writing, max_contiguous tracking, checkpoint commits, and progress logging.
    ///
    /// Performance optimizations:
    /// 1. Keeps fetchers bounded by writer throughput
    /// 2. Runs blocking database writes off the async runtime
    /// 3. Sorts account/storage writes inside archive batches before commit
    /// 4. Prefers APPEND where keys are strictly increasing, falling back to UPSERT
    fn spawn_checkpoint_worker(
        mut rx: mpsc::Receiver<EncodedBlockData>,
        db: Arc<ArchiveStorage>,
        start_block: u64,
        total_blocks: u64,
        overall_start: Instant,
        checkpoint_interval: u64,
    ) -> tokio::task::JoinHandle<(u64, u64)> {
        tokio::task::spawn_blocking(move || {
            // Pending blocks waiting to be written (out-of-order arrivals)
            let mut pending_blocks: BTreeMap<u64, EncodedBlockData> = BTreeMap::new();
            // Use Option to correctly handle start_block = 0 case
            let mut max_contiguous: Option<u64> = if start_block == 0 {
                None
            } else {
                Some(start_block - 1)
            };
            let mut last_checkpoint_num = start_block.saturating_sub(1) / checkpoint_interval;
            let mut count: u64 = 0;
            let mut written_count: u64 = 0;
            let mut batch_dirty = false;
            let mut last_written_hash: Option<H256> = None;

            // Current batch for accumulating writes
            let mut current_batch = db
                .prepare_write_batch()
                .expect("Failed to prepare initial batch");

            while let Some(block_data) = rx.blocking_recv() {
                count += 1;
                let block_num = block_data.block_num;

                // Store the block data
                if max_contiguous.is_some_and(|mc| block_num <= mc) {
                    warn!(target: "archive_init",
                        "Duplicate block {} received after it was already written; ignoring",
                        block_num);
                    continue;
                }
                if pending_blocks.insert(block_num, block_data).is_some() {
                    warn!(target: "archive_init",
                        "Duplicate pending block {} received; replacing previous data",
                        block_num);
                }

                // Write blocks in order starting from max_contiguous + 1 (or start_block)
                let write_start = match max_contiguous {
                    Some(mc) => mc + 1,
                    None => start_block,
                };

                // Write all consecutive blocks we have
                let mut next_to_write = write_start;
                while let Some(block) = pending_blocks.remove(&next_to_write) {
                    let block_hash = block.block_hash;

                    // Write block data to batch (archive batches sort at commit time)
                    Self::write_block_to_batch(&db, &mut current_batch, block)
                        .expect("Failed to write block to batch");

                    max_contiguous = Some(next_to_write);
                    written_count += 1;
                    batch_dirty = true;
                    last_written_hash = Some(block_hash);

                    let current_checkpoint_num = next_to_write / checkpoint_interval;
                    if current_checkpoint_num > last_checkpoint_num {
                        db.write_latest_block_hash(&mut current_batch, block_hash)
                            .expect("Failed to write latest block hash");
                        // commit_sync fsyncs the WAL (RocksDB) or env (MDBX)
                        // before returning, so a mid-ingest crash resumes from
                        // here. Replaces the previous explicit flush() which
                        // forced a synchronous memtable→SST conversion across
                        // every CF (~18s per checkpoint). WAL fsync is ms-level
                        // and replay on reopen restores the memtables.
                        db.commit_sync(current_batch)
                            .expect("Failed to commit batch (sync)");

                        last_checkpoint_num = current_checkpoint_num;
                        batch_dirty = false;
                        info!(target: "archive_init",
                            "Checkpoint committed at block {} (written: {})",
                            next_to_write, written_count);

                        current_batch = db
                            .prepare_write_batch()
                            .expect("Failed to prepare new batch");
                    }

                    next_to_write += 1;
                }

                // Log progress every 100 blocks received
                if count % 100 == 0 {
                    let elapsed = overall_start.elapsed().as_secs_f64();
                    let blocks_per_sec = count as f64 / elapsed;
                    let remaining = total_blocks.saturating_sub(count);
                    let eta_secs = if blocks_per_sec > 0.0 {
                        (remaining as f64 / blocks_per_sec) as u64
                    } else {
                        0
                    };
                    let progress_pct = (count.min(total_blocks) * 100) / total_blocks;
                    let mc_display = max_contiguous.map(|v| v as i64).unwrap_or(-1);
                    let pending_count = pending_blocks.len();

                    info!(target: "archive_init",
                        "Progress: {}% ({}/{}) | Written: {} | Contiguous: {} | Pending: {} | Speed: {:.1} blocks/s | ETA: {}s",
                        progress_pct, count, total_blocks, written_count, mc_display, pending_count,
                        blocks_per_sec, eta_secs);
                }
            }

            // Write final checkpoint - commit any remaining blocks in batch
            let final_contiguous = max_contiguous.unwrap_or(start_block.saturating_sub(1));

            if batch_dirty {
                if let Some(last_hash) = last_written_hash {
                    db.write_latest_block_hash(&mut current_batch, last_hash)
                        .expect("Failed to write final latest block hash");
                }
                db.commit_sync(current_batch)
                    .expect("Failed to commit final batch (sync)");
                info!(target: "archive_init", "Final checkpoint written at block {}", final_contiguous);
            } else {
                // No pending writes. Dropping the prepared batch aborts the empty
                // MDBX transaction before the final database sync.
                drop(current_batch);
            }
            db.flush().expect("Failed to flush database");

            (written_count, final_contiguous)
        })
    }

    /// Write a single block to the provided batch (without committing).
    ///
    /// Account/storage entries are pre-encoded by the fetcher and pushed
    /// straight into the batch's deferred cache here. Block-info / block-hash
    /// / code writes go through the regular trait API.
    fn write_block_to_batch(
        db: &Arc<ArchiveStorage>,
        batch: &mut ArchiveWriteBatch,
        block: EncodedBlockData,
    ) -> Result<()> {
        let EncodedBlockData {
            block_num: _,
            block_hash,
            block_info,
            accounts,
            storage,
            codes,
        } = block;

        db.write_block_hash(batch, block_info.header.number, block_hash)?;
        db.write_block_info(batch, block_info)?;

        db.extend_account_writes(batch, accounts)?;
        db.extend_storage_writes(batch, storage)?;

        for code in codes {
            db.write_code(batch, code.code_hash, code.code)?;
        }

        Ok(())
    }

    /// Get the start block number, checking for existing data to support resume
    fn get_start_block(&self, db: &Arc<ArchiveStorage>) -> Result<u64> {
        let latest_hash = db.read_latest_block_hash()?;
        if latest_hash == H256::ZERO {
            // Database is empty, start from 0
            Ok(0)
        } else {
            // Find the latest block number
            let latest_block = db.read_block_info(latest_hash)?;
            match latest_block {
                Some(block) => {
                    let next_block = block.header.number + 1;
                    info!(target: "archive_init", "Resuming from block {} (last committed: {})",
                          next_block, block.header.number);
                    Ok(next_block)
                }
                None => {
                    // Latest hash exists but block info not found - database is corrupted
                    anyhow::bail!(
                        "Database is in an inconsistent state: latest block hash exists but block info not found. \
                        Please delete the database directory and retry."
                    );
                }
            }
        }
    }

    /// Fetch block data from RPC/S3 with retry logic (writing is done by checkpoint worker)
    async fn fetch_block_with_retry(
        rpc_client: Option<HttpClient>,
        s3_client: Client,
        bucket: String,
        outer_bucket: String,
        chain_id: String,
        version: String,
        block_num: u64,
    ) -> EncodedBlockData {
        let mut last_error = String::new();

        for attempt in 1..=MAX_RETRIES {
            match Self::fetch_block(
                rpc_client.clone(),
                s3_client.clone(),
                bucket.clone(),
                outer_bucket.clone(),
                chain_id.clone(),
                version.clone(),
                block_num,
            )
            .await
            {
                Ok(result) => return result,
                Err(e) => {
                    last_error = e.to_string();
                    if attempt < MAX_RETRIES {
                        warn!(target: "archive_init",
                            "Block {} fetch failed (attempt {}/{}): {}. Retrying...",
                            block_num, attempt, MAX_RETRIES, last_error);
                        sleep(RETRY_DELAY).await;
                    }
                }
            }
        }

        // All retries failed, panic
        panic!(
            "Block {} fetch failed after {} retries. Last error: {}",
            block_num, MAX_RETRIES, last_error
        );
    }

    /// Fetch block data from RPC/S3 and pre-encode it for the writer.
    ///
    /// Encoding (`encode_account_key`, `encode_storage_key`, RLP of
    /// `SlimAccount`, `U256::to_be_bytes`) runs here so it scales with the
    /// fetcher concurrency rather than serializing on the single writer
    /// thread.
    async fn fetch_block(
        rpc_client: Option<HttpClient>,
        s3_client: Client,
        bucket: String,
        outer_bucket: String,
        chain_id: String,
        version: String,
        block_num: u64,
    ) -> Result<EncodedBlockData> {
        let (block_info, block_diff) = if block_num == 0 {
            // Genesis block has no parent
            s3_get_block_info_and_diff_by_number_for_genesis(
                &rpc_client,
                &s3_client,
                &bucket,
                &outer_bucket,
                &chain_id,
                &version,
                block_num,
            )
            .await?
        } else {
            s3_get_block_info_and_diff_by_number(
                &rpc_client,
                &s3_client,
                &bucket,
                &outer_bucket,
                &chain_id,
                &version,
                block_num,
            )
            .await?
        };

        let block_hash = block_info.header.hash;

        let mut accounts = Vec::with_capacity(
            block_diff.deleted_accounts.len() + block_diff.new_accounts.len(),
        );
        for address in block_diff.deleted_accounts {
            accounts.push((encode_account_key(address, block_num), None));
        }
        for account in block_diff.new_accounts {
            let key = encode_account_key(account.address, block_num);
            let value = encode_slim_account(account);
            accounts.push((key, Some(value)));
        }

        let storage_count: usize = block_diff
            .storage_diffs
            .iter()
            .map(|d| d.diffs.len())
            .sum();
        let mut storage = Vec::with_capacity(storage_count);
        for account_diff in block_diff.storage_diffs {
            for pair in account_diff.diffs {
                let key = encode_storage_key(account_diff.address, pair.index, block_num);
                let value: [u8; 32] = pair.value.to_be_bytes();
                storage.push((key, value));
            }
        }

        Ok(EncodedBlockData {
            block_num,
            block_hash,
            block_info,
            accounts,
            storage,
            codes: block_diff.new_codes,
        })
    }
}
