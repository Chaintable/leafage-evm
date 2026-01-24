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
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
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

        // Create channel for sending ProcessResult to checkpoint worker
        let (tx, rx) = mpsc::unbounded_channel::<ProcessResult>();

        // Spawn checkpoint worker
        let checkpoint_worker = Self::spawn_checkpoint_worker(
            rx,
            db.clone(),
            start_block,
            self.end_block,
            total_blocks,
            overall_start,
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
                        rpc,
                        s3,
                        bucket,
                        outer_bucket,
                        chain_id,
                        version,
                        db,
                        block_num,
                    )
                    .await
                }
            })
            .buffer_unordered(max_tasks)
            .for_each(|result| {
                let tx = tx.clone();
                async move {
                    // Send result to checkpoint worker
                    tx.send(result).expect("Checkpoint worker channel closed");
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

        Ok(())
    }

    /// Spawn checkpoint worker that handles max_contiguous tracking, checkpoint commits, and progress logging
    fn spawn_checkpoint_worker(
        mut rx: mpsc::UnboundedReceiver<ProcessResult>,
        db: Arc<ArchiveRocksDBStorage>,
        start_block: u64,
        end_block: u64,
        total_blocks: u64,
        overall_start: Instant,
    ) -> tokio::task::JoinHandle<(u64, u64)> {
        tokio::spawn(async move {
            let mut completed: BTreeMap<u64, H256> = BTreeMap::new();
            // Use Option to correctly handle start_block = 0 case
            let mut max_contiguous: Option<u64> = if start_block == 0 {
                None
            } else {
                Some(start_block - 1)
            };
            let mut last_checkpoint_num = start_block.saturating_sub(1) / CHECKPOINT_INTERVAL;
            let mut count: u64 = 0;

            while let Some(result) = rx.recv().await {
                count += 1;

                // Record completion
                completed.insert(result.block_num, result.block_hash);

                // Update max_contiguous by scanning from start_block or current max + 1
                let scan_start = match max_contiguous {
                    Some(mc) => mc + 1,
                    None => start_block,
                };
                let mut current = max_contiguous;
                let mut next = scan_start;
                while completed.contains_key(&next) {
                    current = Some(next);
                    next += 1;
                }
                max_contiguous = current;

                // Check if we can write a new checkpoint
                if let Some(mc) = max_contiguous {
                    let current_checkpoint_num = mc / CHECKPOINT_INTERVAL;
                    if current_checkpoint_num > last_checkpoint_num {
                        let checkpoint_block = current_checkpoint_num * CHECKPOINT_INTERVAL;
                        let block_hash = completed[&checkpoint_block];
                        Self::commit_checkpoint(&db, block_hash);
                        last_checkpoint_num = current_checkpoint_num;
                        info!(target: "archive_init",
                            "Checkpoint written at block {} (max_contiguous: {})",
                            checkpoint_block, mc);

                        // Clean up completed blocks to save memory
                        let cleanup_threshold = last_checkpoint_num * CHECKPOINT_INTERVAL;
                        if cleanup_threshold > start_block {
                            completed.retain(|&k, _| k >= cleanup_threshold);
                        }
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
                    let mc_display = max_contiguous.map(|v| v as i64).unwrap_or(-1);

                    info!(target: "archive_init",
                        "Progress: {}% ({}/{}) | Block {} | Contiguous: {} | Speed: {:.1} blocks/s | ETA: {}s",
                        progress_pct, count, total_blocks, result.block_num, mc_display,
                        blocks_per_sec, eta_secs);
                }
            }

            // Write final checkpoint
            let final_contiguous = max_contiguous.unwrap_or(start_block.saturating_sub(1));
            if final_contiguous >= end_block {
                let block_hash = completed[&end_block];
                Self::commit_checkpoint(&db, block_hash);
                info!(target: "archive_init", "Final checkpoint written at block {}", end_block);
            }

            (count, final_contiguous)
        })
    }

    /// Commit a checkpoint to the database
    fn commit_checkpoint(db: &Arc<ArchiveRocksDBStorage>, block_hash: H256) {
        let mut batch = db.prepare_write_batch().expect("Failed to prepare batch");
        db.write_latest_block_hash(&mut batch, block_hash)
            .expect("Failed to write latest block hash");
        db.commit(batch).expect("Failed to commit checkpoint");
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
