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

    fn commit(&self, batch: ArchiveWriteBatch) -> Result<(), anyhow::Error> {
        match (self, batch) {
            (ArchiveStorage::RocksDB(db), ArchiveWriteBatch::RocksDB(b)) => Ok(db.commit(b)?),
            (ArchiveStorage::MDBX(db), ArchiveWriteBatch::MDBX(b)) => Ok(db.commit(b)?),
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

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        info!(target: "archive_init", "Starting archive initialization");
        info!(target: "archive_init", "db_path: {:?}, rpc_addr: {}, end_block: {}, max_tasks: {}, db_type: {:?}",
              self.db_path, self.rpc_addr, self.end_block, self.max_tasks, self.db_type);

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

        // Create a bounded channel so fetchers cannot run arbitrarily far ahead
        // of the single ordered database writer.
        let (tx, rx) = mpsc::channel::<EncodedBlockData>(self.max_tasks);

        // Spawn checkpoint worker
        let checkpoint_worker = Self::spawn_checkpoint_worker(
            rx,
            db.clone(),
            start_block,
            total_blocks,
            overall_start,
            self.checkpoint_interval,
        );

        // Create stream of block heights
        let blocks = stream::iter(start_block..=self.end_block);

        // Capture variables for the async block
        let rpc_client = Some(rpc_client);
        let bucket = self.s3_bucket.clone();
        let outer_bucket = self.s3_outer_bucket.clone();
        let chain_id = self.s3_chain_id.clone();
        let version = self.s3_version.clone();
        let max_tasks = self.max_tasks;

        // Process blocks concurrently with buffer_unordered (fetch only)
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
                    // Send block data to checkpoint worker for writing
                    tx.send(block_data)
                        .await
                        .expect("Checkpoint worker channel closed");
                }
            })
            .await;

        // Drop the sender to signal completion to the checkpoint worker
        drop(tx);

        // Wait for checkpoint worker to finish and get final stats
        let (final_success, final_contiguous) = checkpoint_worker.await?;

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
            let compact_db = ArchiveRocksDBStorage::open(&self.db_path, self.db_cache, false);
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
                        db.commit(current_batch).expect("Failed to commit batch");
                        // Force durability so the resume pointer survives a
                        // mid-ingest crash. Without WAL (bulk-load) and with
                        // UtterlyNoSync MDBX, commit() lands in memory only;
                        // the LatestBlockHash CF in particular is too small to
                        // ever trigger memtable auto-flush. flush() flushes
                        // every CF in the right order (content CFs first,
                        // pointer last) so a crash mid-flush either recovers
                        // to an earlier checkpoint or to this one — never to
                        // a pointer that outruns its content.
                        db.flush().expect("Failed to flush at checkpoint");

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
                db.commit(current_batch)
                    .expect("Failed to commit final batch");
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
