//! Offline export of pipeline bundles, as `pipeline-compactor` builds them,
//! for any chain with a [`PipelineChain`] replay, uploaded to the compactor's
//! target objects.
//!
//! Bundle `b` holds the blocks `(b - 1) * 1000 + 1 ..= b * 1000`, bundle 0
//! only the genesis block, which is made from its alloc rather than replayed
//! (see [`super::genesis`]). Any of its
//! four resources (validation, block file, header, state diff, see
//! [`super::bundle`]) are built in memory and uploaded; nothing is written to
//! disk. The replay only produces what they need: the block file and the
//! validation need the transactions traced, the state diff needs the state
//! changes recorded. Uploaded objects carry a marker naming the export of
//! their bundle, so a rerun skips the bundles whose objects were all uploaded
//! by one export. A bundle that fails is logged and the export
//! moves on; the export fails at the end, naming them.
//!
//! Workers replay runs of consecutive bundles. The bodies of the next chunk
//! of blocks are fetched while the current one is replayed, and the replay
//! state carries over from one bundle of a run to the next, rebased on the
//! archive every `rebase_every` blocks to bound its memory: keys a run read
//! once are not read from the archive again.

use super::blocks::{fetch_raw_blocks, FetchedBlock};
use super::bundle::{BundleObjects, BundleWriter, Resource, BUNDLE_SIZE};
use super::chain::{Chain, ChainReplay, PipelineChain, ReplayOptions};
use super::s3::{MultipartUpload, S3Target};
use crate::api_impl::arc::archive_replay::ArcPipeline;
use crate::api_impl::bsc::trace::BscPipeline;
use crate::api_impl::mainnet::trace::EthPipeline;
use anyhow::{anyhow, Context as _, Result};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_storage::{BlockIndex, EvmStorageRead};
use leafage_evm_types::{BlockId, BlockInfo, BlockNumberOrTag};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn};

#[derive(Clone)]
pub struct ExportConfig {
    /// Upstream RPC serving full block bodies.
    pub block_rpc: String,
    /// First and last bundle to export, inclusive.
    pub from_bundle: u64,
    pub to_bundle: u64,
    /// The resources to export.
    pub resources: BTreeSet<Resource>,
    /// Where the bundles go.
    pub s3: Arc<S3Target>,
    /// gzip level of the compressed bundles.
    pub gzip_level: u32,
    /// Export bundles this tool already uploaded again.
    pub overwrite: bool,
    /// Bundles exported at the same time.
    pub workers: usize,
    /// Consecutive bundles a worker exports on one replay state.
    pub run_bundles: u64,
    /// Blocks fetched upstream in one batch.
    pub chunk: u64,
    /// Blocks replayed on one cached state before going back to the archive
    /// state, across the bundles of a run; a multiple of `chunk`. Larger
    /// saves reads, costs memory.
    pub rebase_every: u64,
    /// Compare the replayed state changes with the archive (slow).
    pub check_state: bool,
}

impl ExportConfig {
    pub(crate) fn replay_options(&self) -> ReplayOptions {
        let has = |resource| self.resources.contains(&resource);
        ReplayOptions {
            strict: true,
            check_state: self.check_state,
            state_diff: has(Resource::StateDiff),
            traces: has(Resource::BlockFile) || has(Resource::Validation),
        }
    }

    /// Whether bundle `id` needs no export.
    async fn is_done(&self, id: u64) -> bool {
        if self.overwrite {
            return false;
        }
        match self.s3.is_uploaded(id, &self.resources).await {
            Ok(uploaded) => uploaded,
            Err(err) => {
                warn!(target: "export", bundle = id, "exporting the bundle: {err:#}");
                false
            }
        }
    }
}

/// Height range of bundle `id`; bundle 0 is the genesis block alone.
pub fn bundle_range(id: u64) -> (u64, u64) {
    if id == 0 {
        return (0, 0);
    }
    ((id - 1) * BUNDLE_SIZE + 1, id * BUNDLE_SIZE)
}

/// Totals over the whole export, for progress lines.
#[derive(Default)]
struct Progress {
    blocks: AtomicU64,
    txs: AtomicU64,
    bytes: AtomicU64,
    bundles: AtomicU64,
    skipped: AtomicU64,
    failed: Mutex<Vec<u64>>,
}

/// Export the bundles of `config` from the archive `db` of `chain`.
pub async fn export_bundles<DB>(db: Arc<DB>, chain: Chain, config: ExportConfig) -> Result<()>
where
    DB: EvmStorageRead + BlockIndex + Send + Sync + 'static,
    DB::StateDB: Send + 'static,
{
    match chain {
        Chain::Arc => export_chain(db, Arc::new(ArcPipeline), config).await,
        Chain::Bsc => export_chain(db, Arc::new(BscPipeline), config).await,
        Chain::Eth => export_chain(db, Arc::new(EthPipeline), config).await,
    }
}

async fn export_chain<DB, C>(db: Arc<DB>, chain: Arc<C>, config: ExportConfig) -> Result<()>
where
    DB: Send + Sync + BlockIndex + 'static,
    C: PipelineChain<DB>,
{
    anyhow::ensure!(config.from_bundle <= config.to_bundle, "empty bundle range");
    anyhow::ensure!(!config.resources.is_empty(), "no resource to export");
    for &resource in &config.resources {
        config.s3.object(1, resource)?;
    }
    anyhow::ensure!(
        config.s3.part_size >= 5 << 20,
        "the part size must be at least 5 MiB"
    );
    anyhow::ensure!(
        config.chunk > 0 && BUNDLE_SIZE.is_multiple_of(config.chunk),
        "the chunk must divide {BUNDLE_SIZE}"
    );
    anyhow::ensure!(
        config.rebase_every > 0 && config.rebase_every.is_multiple_of(config.chunk),
        "rebase_every must be a multiple of the chunk"
    );
    anyhow::ensure!(config.run_bundles > 0, "run_bundles must be positive");
    anyhow::ensure!(config.workers > 0, "workers must be positive");
    let client = HttpClientBuilder::default()
        .request_timeout(Duration::from_secs(300))
        .max_response_size(u32::MAX)
        .build(&config.block_rpc)
        .with_context(|| format!("invalid block rpc {:?}", config.block_rpc))?;
    info!(
        target: "export",
        resources = ?config.resources,
        prefix = %config.s3.prefix,
        options = ?config.replay_options(),
        "exporting"
    );

    let config = Arc::new(config);
    let next = Arc::new(AtomicU64::new(config.from_bundle));
    let progress = Arc::new(Progress::default());
    let started = Instant::now();
    let reporter = {
        let progress = progress.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                log_progress(&progress, started.elapsed());
            }
        })
    };
    let workers: Vec<_> = (0..config.workers)
        .map(|_| {
            let (db, chain, client, config, next, progress) = (
                db.clone(),
                chain.clone(),
                client.clone(),
                config.clone(),
                next.clone(),
                progress.clone(),
            );
            tokio::spawn(async move {
                loop {
                    let start = next.fetch_add(config.run_bundles, Ordering::Relaxed);
                    if start > config.to_bundle {
                        return;
                    }
                    let end = (start + config.run_bundles - 1).min(config.to_bundle);
                    let mut carry = None;
                    for id in start..=end {
                        if config.is_done(id).await {
                            progress.skipped.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        let result = export_bundle(
                            &db, &*chain, &client, &config, id, &progress, &mut carry,
                        )
                        .await;
                        if let Err(err) = result {
                            error!(target: "export", bundle = id, "bundle failed: {err:#}");
                            progress.failed.lock().expect("not poisoned").push(id);
                        }
                    }
                }
            })
        })
        .collect();
    for worker in workers {
        worker.await?;
    }
    reporter.abort();
    log_progress(&progress, started.elapsed());
    let mut failed = std::mem::take(&mut *progress.failed.lock().expect("not poisoned"));
    failed.sort_unstable();
    anyhow::ensure!(
        failed.is_empty(),
        "{} bundles failed (rerun to retry them): {failed:?}",
        failed.len()
    );
    Ok(())
}

fn log_progress(progress: &Progress, elapsed: Duration) {
    let blocks = progress.blocks.load(Ordering::Relaxed);
    let secs = elapsed.as_secs_f64().max(1e-3);
    info!(
        target: "export",
        bundles = progress.bundles.load(Ordering::Relaxed),
        skipped = progress.skipped.load(Ordering::Relaxed),
        failed = progress.failed.lock().expect("not poisoned").len(),
        blocks,
        txs = progress.txs.load(Ordering::Relaxed),
        mb = progress.bytes.load(Ordering::Relaxed) / 1_000_000,
        blocks_per_sec = format!("{:.1}", blocks as f64 / secs),
        blocks_per_day_m = format!("{:.2}", blocks as f64 / secs * 86400.0 / 1e6),
        "progress"
    );
}

/// The replay after a bundle, for the next bundle of the run.
struct Carry<R> {
    replay: R,
    /// The last block replayed.
    number: u64,
    /// Blocks replayed since the replay state was last rebased.
    since_rebase: u64,
}

/// Export bundle `id`, continuing on the replay `carry` left by the
/// previous bundle when that ends right before this one. `carry` is left
/// with this bundle's replay, or empty if it fails.
async fn export_bundle<DB, C>(
    db: &Arc<DB>,
    chain: &C,
    client: &HttpClient,
    config: &ExportConfig,
    id: u64,
    progress: &Progress,
    carry: &mut Option<Carry<C::Replay>>,
) -> Result<()>
where
    DB: Send + Sync + BlockIndex + 'static,
    C: PipelineChain<DB>,
{
    if id == 0 {
        // Nothing to carry over from or to the genesis block.
        *carry = None;
        return export_genesis(db, chain, client, config, progress).await;
    }
    // The block file is uploaded in parts as it is built.
    let mut upload = None;
    let result =
        replay_and_upload(db, chain, client, config, id, progress, carry, &mut upload).await;
    if result.is_err() {
        if let Some(upload) = upload {
            upload.abort().await;
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn replay_and_upload<DB, C>(
    db: &Arc<DB>,
    chain: &C,
    client: &HttpClient,
    config: &ExportConfig,
    id: u64,
    progress: &Progress,
    carry: &mut Option<Carry<C::Replay>>,
    upload: &mut Option<MultipartUpload>,
) -> Result<()>
where
    DB: Send + Sync + BlockIndex + 'static,
    C: PipelineChain<DB>,
{
    let started = Instant::now();
    let export = export_id();
    let (first, last) = bundle_range(id);
    let (replay, mut since_rebase) = match carry.take() {
        Some(carried) if carried.number == first - 1 => (carried.replay, carried.since_rebase),
        _ => {
            let parent = block_info(db.as_ref(), first - 1)?;
            (chain.start(db, &parent, config.replay_options())?, 0)
        }
    };
    let counters_before = replay.counters();
    let mut replay = Some(replay);
    let mut writer = Some(BundleWriter::new(id, &config.resources, config.gzip_level));
    let (mut txs, mut bytes) = (0u64, 0u64);
    let (mut fetch_wait, mut execute, mut write, mut upload_wait) = (
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
    );

    let chunks: Vec<(u64, u64)> = (first..=last)
        .step_by(config.chunk as usize)
        .map(|lo| (lo, (lo + config.chunk - 1).min(last)))
        .collect();
    let fetch = |(lo, hi): (u64, u64)| {
        let (db, client) = (db.clone(), client.clone());
        tokio::spawn(async move { fetch_chunk(db, &client, lo, hi).await })
    };
    let mut pending = Some(fetch(chunks[0]));
    for (index, &(lo, hi)) in chunks.iter().enumerate() {
        let waited = Instant::now();
        let (infos, blocks) = pending
            .take()
            .expect("prefetched")
            .await?
            .with_context(|| format!("fetch blocks {lo}..={hi}"))?;
        fetch_wait += waited.elapsed();
        if let Some(&next) = chunks.get(index + 1) {
            pending = Some(fetch(next));
        }

        // Go back to the archive state after this chunk, before the next.
        since_rebase += hi - lo + 1;
        let rebase = since_rebase >= config.rebase_every;
        if rebase {
            since_rebase = 0;
        }
        let (mut chunk_replay, mut chunk_writer) = (
            replay.take().expect("replay"),
            writer.take().expect("writer"),
        );
        let done = tokio::task::spawn_blocking(move || -> Result<_> {
            let (mut txs, mut execute, mut write) = (0, Duration::ZERO, Duration::ZERO);
            for (info, block) in infos.iter().zip(blocks) {
                let number = info.header.number;
                let t = Instant::now();
                let output = chunk_replay
                    .replay(info, block)
                    .with_context(|| format!("replay block {number}"))?;
                execute += t.elapsed();
                let t = Instant::now();
                chunk_writer.push(&output)?;
                txs += output.block_file.txs.len() as u64;
                write += t.elapsed();
            }
            if rebase {
                chunk_replay.rebase(infos.last().expect("chunk"))?;
            }
            Ok((chunk_replay, chunk_writer, txs, execute, write))
        })
        .await??;
        let (chunk_replay, mut chunk_writer, chunk_txs, chunk_execute, chunk_write) = done;
        if let Some(part) = chunk_writer.take_block_file_part(config.s3.part_size) {
            let t = Instant::now();
            bytes += part.len() as u64;
            if upload.is_none() {
                *upload = Some(
                    config
                        .s3
                        .create_multipart(id, &export, Resource::BlockFile)
                        .await?,
                );
            }
            upload
                .as_mut()
                .expect("created")
                .put_part(part.into())
                .await?;
            upload_wait += t.elapsed();
        }
        replay = Some(chunk_replay);
        writer = Some(chunk_writer);
        txs += chunk_txs;
        execute += chunk_execute;
        write += chunk_write;
        progress.blocks.fetch_add(hi - lo + 1, Ordering::Relaxed);
        progress.txs.fetch_add(chunk_txs, Ordering::Relaxed);
    }

    let writer = writer.expect("writer");
    let objects = tokio::task::spawn_blocking(move || writer.finish()).await??;
    let t = Instant::now();
    bytes += upload_objects(config, id, &export, objects, upload).await?;
    upload_wait += t.elapsed();

    progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    progress.bundles.fetch_add(1, Ordering::Relaxed);
    let replay = replay.expect("replay");
    // The chain's counters over this bundle.
    let counters: Vec<_> = replay
        .counters()
        .into_iter()
        .zip(counters_before)
        .map(|((name, after), (_, before))| format!("{name}: {}", after - before))
        .collect();
    info!(
        target: "export",
        bundle = id,
        txs,
        mb = format!("{:.1}", bytes as f64 / 1e6),
        total_ms = started.elapsed().as_millis() as u64,
        fetch_wait_ms = fetch_wait.as_millis() as u64,
        execute_ms = execute.as_millis() as u64,
        write_ms = write.as_millis() as u64,
        upload_ms = upload_wait.as_millis() as u64,
        counters = counters.join(", "),
        "bundle done"
    );
    *carry = Some(Carry {
        replay,
        number: last,
        since_rebase,
    });
    Ok(())
}

/// Tells the objects of a bundle uploaded by one export from those of other
/// runs.
fn export_id() -> String {
    format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

/// Upload the finished `objects` of bundle `id` in the compactor's order,
/// completing the block file's `upload` if it was started; the bytes sent.
async fn upload_objects(
    config: &ExportConfig,
    id: u64,
    export: &str,
    objects: BundleObjects,
    upload: &mut Option<MultipartUpload>,
) -> Result<u64> {
    let mut bytes = 0;
    if let Some(data) = objects.validation {
        bytes += data.len() as u64;
        config
            .s3
            .put(id, export, Resource::Validation, data.into())
            .await?;
    }
    if let Some(data) = objects.block_file {
        bytes += data.len() as u64;
        match upload {
            Some(started) => {
                // Left in `upload` to be aborted if it fails.
                if !data.is_empty() {
                    started.put_part(data.into()).await?;
                }
                // Aborts the upload itself if it fails.
                upload.take().expect("started").complete().await?;
            }
            None => {
                config
                    .s3
                    .put(id, export, Resource::BlockFile, data.into())
                    .await?
            }
        }
    }
    if let Some(data) = objects.header {
        bytes += data.len() as u64;
        config
            .s3
            .put(id, export, Resource::Header, data.into())
            .await?;
    }
    if let Some(data) = objects.state_diff {
        bytes += data.len() as u64;
        config
            .s3
            .put(id, export, Resource::StateDiff, data.into())
            .await?;
    }
    Ok(bytes)
}

/// Export bundle 0, the genesis block.
async fn export_genesis<DB, C>(
    db: &Arc<DB>,
    chain: &C,
    client: &HttpClient,
    config: &ExportConfig,
    progress: &Progress,
) -> Result<()>
where
    DB: Send + Sync + BlockIndex + 'static,
    C: PipelineChain<DB>,
{
    let (infos, blocks) = fetch_chunk(db.clone(), client, 0, 0)
        .await
        .context("fetch the genesis block")?;
    let (info, block) = infos
        .first()
        .zip(blocks.first())
        .context("no genesis block")?;
    let output = chain.genesis(db, info, &block.block, config.replay_options())?;
    let mut writer = BundleWriter::new(0, &config.resources, config.gzip_level);
    writer.push(&output)?;
    let txs = output.block_file.txs.len() as u64;
    let bytes = upload_objects(config, 0, &export_id(), writer.finish()?, &mut None).await?;
    progress.blocks.fetch_add(1, Ordering::Relaxed);
    progress.txs.fetch_add(txs, Ordering::Relaxed);
    progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    progress.bundles.fetch_add(1, Ordering::Relaxed);
    info!(target: "export", bundle = 0, txs, bytes, "bundle done");
    Ok(())
}

fn block_info<DB: BlockIndex>(db: &DB, number: u64) -> Result<Arc<BlockInfo>> {
    db.get_block_by_id_arc(BlockId::Number(BlockNumberOrTag::Number(number)))
        .map_err(|err| anyhow!("block {number}: {err}"))?
        .ok_or_else(|| anyhow!("block {number} is not in the archive"))
}

async fn fetch_chunk<DB: BlockIndex + Send + Sync + 'static>(
    db: Arc<DB>,
    client: &HttpClient,
    lo: u64,
    hi: u64,
) -> Result<(Vec<Arc<BlockInfo>>, Vec<FetchedBlock>)> {
    // Archive reads block; keep them off the async workers.
    let infos = tokio::task::spawn_blocking(move || {
        (lo..=hi)
            .map(|number| block_info(db.as_ref(), number))
            .collect::<Result<Vec<_>>>()
    })
    .await??;
    let hashes: Vec<_> = infos.iter().map(|info| info.header.hash).collect();
    let blocks = fetch_raw_blocks(client, &hashes).await?;
    Ok((infos, blocks))
}
