use crate::utils::{
    s3_get_block_info_and_diff_by_number, s3_get_block_info_and_diff_by_number_for_genesis,
};
use anyhow::Result;
use aws_sdk_s3::Client;
use clap::Parser;
use futures::{stream, StreamExt};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_storage::{ArchiveRocksDBStorage, StateDBWrite};
use leafage_evm_types::{Block, BlockStorageDiff, H256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{info, warn};

/// Checkpoint interval for writing latest block hash
const CHECKPOINT_INTERVAL: u64 = 10240;

/// Maximum retry attempts for failed blocks
const MAX_RETRIES: u32 = 3;

/// Delay between retries
const RETRY_DELAY: Duration = Duration::from_secs(1);

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

    /// Database cache size in MB
    #[arg(long, default_value = "2048")]
    db_cache: usize,

    /// Max concurrent tasks for fetching and writing
    #[arg(long, default_value = "256")]
    max_tasks: usize,
}

struct ProcessResult {
    block_num: u64,
    block_hash: H256,
}

/// Tracks completed blocks and computes the maximum contiguous completed block
struct CompletionTracker {
    start_block: u64,
    /// Maps block_num -> block_hash for completed blocks
    /// Also stores max_contiguous to ensure atomic updates under the same lock
    inner: Mutex<CompletionTrackerInner>,
}

struct CompletionTrackerInner {
    completed: BTreeMap<u64, H256>,
    /// The highest block number where all blocks from start_block to this block are completed
    max_contiguous: u64,
    /// Last written checkpoint number (for cleanup)
    last_written_checkpoint: u64,
}

impl CompletionTracker {
    fn new(start_block: u64) -> Self {
        Self {
            start_block,
            inner: Mutex::new(CompletionTrackerInner {
                completed: BTreeMap::new(),
                max_contiguous: start_block.saturating_sub(1),
                last_written_checkpoint: (start_block.saturating_sub(1)) / CHECKPOINT_INTERVAL,
            }),
        }
    }

    /// Record a completed block and update max_contiguous if possible
    /// Returns the new max_contiguous value
    fn record_completion(&self, block_num: u64, block_hash: H256) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.completed.insert(block_num, block_hash);

        // Update max_contiguous by scanning from current max + 1
        // This is now atomic with the insert since we hold the lock
        let mut current = inner.max_contiguous;
        loop {
            let next = current + 1;
            if inner.completed.contains_key(&next) {
                current = next;
            } else {
                break;
            }
        }
        inner.max_contiguous = current;

        // Clean up completed blocks to save memory
        // Keep blocks from the last written checkpoint onwards to ensure we can read checkpoint hashes
        let cleanup_threshold = inner.last_written_checkpoint * CHECKPOINT_INTERVAL;
        if cleanup_threshold > self.start_block {
            inner.completed.retain(|&k, _| k >= cleanup_threshold);
        }

        current
    }

    /// Mark a checkpoint as written, allowing older data to be cleaned up
    fn mark_checkpoint_written(&self, checkpoint_num: u64) {
        let mut inner = self.inner.lock().unwrap();
        if checkpoint_num > inner.last_written_checkpoint {
            inner.last_written_checkpoint = checkpoint_num;
        }
    }

    /// Get the block hash for a specific block number (if still in memory)
    fn get_block_hash(&self, block_num: u64) -> Option<H256> {
        self.inner
            .lock()
            .unwrap()
            .completed
            .get(&block_num)
            .copied()
    }

    fn max_contiguous(&self) -> u64 {
        self.inner.lock().unwrap().max_contiguous
    }
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        info!(target: "archive_init", "Starting archive initialization");
        info!(target: "archive_init", "db_path: {:?}, rpc_addr: {}, end_block: {}, max_tasks: {}",
              self.db_path, self.rpc_addr, self.end_block, self.max_tasks);

        // Initialize S3 client
        let s3_config = aws_config::load_from_env().await;
        let s3_client = aws_sdk_s3::Client::new(&s3_config);

        // Initialize RPC client
        let rpc_client = HttpClientBuilder::default().build(&self.rpc_addr)?;

        // Open archive database with auto compactions disabled for faster bulk writes
        let db = Arc::new(ArchiveRocksDBStorage::open(
            &self.db_path,
            self.db_cache,
            true, // disable_auto_compactions
        ));

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

        // Counter for progress tracking
        let success_count = Arc::new(AtomicU64::new(0));

        // Track completed blocks for checkpoint logic
        let tracker = Arc::new(CompletionTracker::new(start_block));
        let last_checkpoint = Arc::new(AtomicU64::new(
            (start_block.saturating_sub(1)) / CHECKPOINT_INTERVAL,
        ));

        // Create stream of block heights
        let blocks = stream::iter(start_block..=self.end_block);

        // Capture variables for the async block
        let rpc_client = Some(rpc_client);
        let bucket = self.s3_bucket.clone();
        let outer_bucket = self.s3_outer_bucket.clone();
        let chain_id = self.s3_chain_id.clone();
        let version = self.s3_version.clone();
        let max_tasks = self.max_tasks;

        // Process blocks concurrently with buffer_unordered
        blocks
            .map(|block_num| {
                let rpc = rpc_client.clone();
                let s3 = s3_client.clone();
                let bucket = bucket.clone();
                let outer_bucket = outer_bucket.clone();
                let chain_id = chain_id.clone();
                let version = version.clone();
                let db = db.clone();

                async move {
                    Self::fetch_and_write_block_with_retry(
                        rpc, s3, bucket, outer_bucket, chain_id, version, db, block_num,
                    )
                    .await
                }
            })
            .buffer_unordered(max_tasks)
            .for_each(|result| {
                let success_count = success_count.clone();
                let tracker = tracker.clone();
                let last_checkpoint = last_checkpoint.clone();
                let db = db.clone();
                let overall_start = overall_start;

                async move {
                    let count = success_count.fetch_add(1, Ordering::Relaxed) + 1;

                    // Record completion and get the new max contiguous block
                    let max_contiguous = tracker.record_completion(result.block_num, result.block_hash);

                    // Check if we can write a new checkpoint
                    // Checkpoint at block N means all blocks 0..=N are complete
                    let current_checkpoint_num = max_contiguous / CHECKPOINT_INTERVAL;
                    let last_checkpoint_num = last_checkpoint.load(Ordering::Relaxed);

                    if current_checkpoint_num > last_checkpoint_num {
                        let checkpoint_block = current_checkpoint_num * CHECKPOINT_INTERVAL;

                        // Read checkpoint block's hash from database (more reliable than tracker)
                        let checkpoint_hash = db
                            .read_block_hash(checkpoint_block)
                            .expect("Failed to read checkpoint block hash");

                        if checkpoint_hash != H256::ZERO {
                            let mut batch = db.prepare_write_batch().expect("Failed to prepare batch");
                            db.write_latest_block_hash(&mut batch, checkpoint_hash)
                                .expect("Failed to write latest block hash");
                            db.commit(batch).expect("Failed to commit checkpoint");
                            last_checkpoint.store(current_checkpoint_num, Ordering::Relaxed);
                            tracker.mark_checkpoint_written(current_checkpoint_num);
                            info!(target: "archive_init",
                                "Checkpoint written at block {} (max_contiguous: {})",
                                checkpoint_block, max_contiguous);
                        }
                    }

                    // Log progress every 100 blocks
                    if count % 100 == 0 {
                        let elapsed = overall_start.elapsed().as_secs_f64();
                        let blocks_per_sec = count as f64 / elapsed;
                        let remaining = total_blocks - count;
                        let eta_secs = if blocks_per_sec > 0.0 {
                            (remaining as f64 / blocks_per_sec) as u64
                        } else {
                            0
                        };
                        let progress_pct = (count * 100) / total_blocks;

                        info!(target: "archive_init",
                            "Progress: {}% ({}/{}) | Block {} | Contiguous: {} | Speed: {:.1} blocks/s | ETA: {}s",
                            progress_pct, count, total_blocks, result.block_num, max_contiguous,
                            blocks_per_sec, eta_secs);
                    }
                }
            })
            .await;

        // Final statistics
        let final_success = success_count.load(Ordering::Relaxed);
        let total_time = overall_start.elapsed().as_secs_f64();
        let avg_speed = final_success as f64 / total_time;

        // Write final checkpoint - all blocks should be complete now
        let final_contiguous = tracker.max_contiguous();
        if final_contiguous >= self.end_block {
            if let Some(latest_hash) = tracker.get_block_hash(self.end_block) {
                let mut batch = db.prepare_write_batch()?;
                db.write_latest_block_hash(&mut batch, latest_hash)?;
                db.commit(batch)?;
                info!(target: "archive_init", "Final checkpoint written at block {}", self.end_block);
            }
        } else {
            // This shouldn't happen since all blocks should be processed
            panic!(
                "Final contiguous block {} is less than end_block {}",
                final_contiguous, self.end_block
            );
        }

        info!(target: "archive_init",
            "Archive initialization completed. Total: {} blocks in {:.1}s ({:.1} blocks/s)",
            final_success, total_time, avg_speed);

        Ok(())
    }

    /// Get the start block number, checking for existing data to support resume
    fn get_start_block(&self, db: &Arc<ArchiveRocksDBStorage>) -> Result<u64> {
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

    /// Fetch block data from RPC/S3 and write to database with retry logic
    async fn fetch_and_write_block_with_retry(
        rpc_client: Option<HttpClient>,
        s3_client: Client,
        bucket: String,
        outer_bucket: String,
        chain_id: String,
        version: String,
        db: Arc<ArchiveRocksDBStorage>,
        block_num: u64,
    ) -> ProcessResult {
        let mut last_error = String::new();

        for attempt in 1..=MAX_RETRIES {
            match Self::fetch_and_write_block(
                rpc_client.clone(),
                s3_client.clone(),
                bucket.clone(),
                outer_bucket.clone(),
                chain_id.clone(),
                version.clone(),
                db.clone(),
                block_num,
            )
            .await
            {
                Ok(result) => return result,
                Err(e) => {
                    last_error = e.to_string();
                    if attempt < MAX_RETRIES {
                        warn!(target: "archive_init",
                            "Block {} failed (attempt {}/{}): {}. Retrying...",
                            block_num, attempt, MAX_RETRIES, last_error);
                        sleep(RETRY_DELAY).await;
                    }
                }
            }
        }

        // All retries failed, panic
        panic!(
            "Block {} failed after {} retries. Last error: {}",
            block_num, MAX_RETRIES, last_error
        );
    }

    /// Fetch block data from RPC/S3 and write to database
    async fn fetch_and_write_block(
        rpc_client: Option<HttpClient>,
        s3_client: Client,
        bucket: String,
        outer_bucket: String,
        chain_id: String,
        version: String,
        db: Arc<ArchiveRocksDBStorage>,
        block_num: u64,
    ) -> Result<ProcessResult> {
        // Step 1: Fetch block data
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

        // Step 2: Write to database (blocking operation)
        tokio::task::spawn_blocking(move || {
            Self::write_block(&db, block_num, block_info, block_diff)
        })
        .await??;

        Ok(ProcessResult {
            block_num,
            block_hash,
        })
    }

    /// Write a single block to the database
    fn write_block(
        db: &Arc<ArchiveRocksDBStorage>,
        block_num: u64,
        block_info: Block<H256>,
        block_diff: BlockStorageDiff,
    ) -> Result<()> {
        let mut batch = db.prepare_write_batch()?;

        // Write block hash mapping
        db.write_block_hash(&mut batch, block_info.header.number, block_info.header.hash)?;

        // Write block info
        db.write_block_info(&mut batch, block_info)?;

        // Write deleted accounts
        for account in block_diff.deleted_accounts {
            db.write_account(&mut batch, account, block_num, None)?;
        }

        // Write new accounts
        for account in block_diff.new_accounts {
            db.write_account(&mut batch, account.address, block_num, Some(account))?;
        }

        // Write storage diffs
        for account_diff in block_diff.storage_diffs {
            for index_value_pair in account_diff.diffs {
                db.write_storage(
                    &mut batch,
                    account_diff.address,
                    index_value_pair.index,
                    block_num,
                    index_value_pair.value,
                )?;
            }
        }

        // Write new codes
        for new_code in block_diff.new_codes {
            db.write_code(&mut batch, new_code.code_hash, new_code.code)?;
        }

        // Commit the batch
        db.commit(batch)?;

        Ok(())
    }
}
