use anyhow::{bail, ensure, Result};
use leafage_evm_rpc::{
    bundle_range, export_bundles, Chain, ExportConfig, Resource, S3Target, BUNDLE_SIZE,
};
use leafage_evm_storage::{MultiStorage, StateTree, StateTreeConfig, StorageKind};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// `leafage-evm pipeline-export` command.
///
/// Replays the blocks of a chain (`--chain`) on its archive database and
/// uploads the bundles `pipeline-compactor backfill` would build for them
/// (1000 blocks each) to the compactor's target objects: any of the
/// validation, block file, header and state diff bundles. The flags follow
/// the compactor's. The replay only does what the chosen resources need:
/// traces for the block file and the validation, state changes for the
/// state diff. Nothing is written to disk.
///
/// Run it **offline**: the DB can only be opened by one process. Bundles
/// it already uploaded are skipped, so it is safe to re-run.
#[derive(Debug, clap::Parser)]
#[command(
    group(clap::ArgGroup::new("start").required(true)),
    group(clap::ArgGroup::new("end").required(true))
)]
pub struct Command {
    /// The chain of the archive: bsc.
    #[arg(long)]
    chain: Chain,

    /// The path to the chain's archive database.
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// Database cache size in MB.
    #[arg(long, default_value = "8192")]
    db_cache: usize,

    /// Must match how the DB is served (see `standalone`).
    #[arg(long, default_value = "false")]
    archive_zstd_compression: bool,

    /// Must match how the DB is served (see `standalone`).
    #[arg(long, default_value = "false")]
    inverted_block_encoding: bool,

    /// Code cache of the archive reads, in MB.
    #[arg(long, default_value = "1024")]
    archive_code_cache_mb: u64,

    /// Upstream RPC of the chain serving full block bodies
    /// (`debug_getRawBlock`).
    #[arg(long, value_name = "URL")]
    block_rpc: String,

    /// First bundle ID to export; bundle 0 is the genesis block alone.
    #[arg(long, group = "start")]
    from_bundle: Option<u64>,

    /// First block height to include, expanded to its complete bundle.
    #[arg(long, group = "start", alias = "from-block")]
    from_block_height: Option<u64>,

    /// Last bundle ID to export (inclusive).
    #[arg(long, group = "end")]
    end_bundle: Option<u64>,

    /// Last block height to include, expanded to its complete bundle.
    #[arg(long, group = "end", alias = "to-block")]
    end_block_height: Option<u64>,

    /// Resources to export: validation, blockfile, header, statediff.
    #[arg(long, value_delimiter = ',', default_value = "validation,blockfile")]
    resources: Vec<Resource>,

    /// AWS Region of the target buckets.
    #[arg(long)]
    region: String,

    /// Business chain ID used in S3 keys; the chain's by default.
    #[arg(long)]
    chain_id: Option<String>,

    /// Pipeline data version used in S3 keys, e.g. `fe751dd0`.
    #[arg(long)]
    version: Option<String>,

    /// Target bucket of the block file and validation bundles.
    #[arg(long)]
    target_chain_table_bucket: Option<String>,

    /// Target bucket of the header and state diff bundles.
    #[arg(long)]
    target_nodex_bucket: Option<String>,

    /// Endpoint of an S3-compatible store (path-style), e.g. for tests.
    #[arg(long, value_name = "URL")]
    s3_endpoint: Option<String>,

    /// Parts of one multipart upload sent at the same time.
    #[arg(long, default_value = "8")]
    s3_concurrency: usize,

    /// Multipart upload part size in bytes for the state diff and block file
    /// bundles.
    #[arg(long, default_value = "67108864")]
    statediff_part_size: usize,

    /// gzip level of the bundles.
    #[arg(long, default_value = "6")]
    gzip_level: u32,

    /// Export bundles already uploaded by this command again, e.g. after a
    /// fix; by default they are skipped.
    #[arg(long, default_value = "false")]
    overwrite: bool,

    /// Bundles exported at the same time. Replays wait on archive reads, so
    /// a few more than the CPUs keeps them busy (32 on 12 vCPUs).
    #[arg(long, default_value = "32", value_parser = clap::value_parser!(u64).range(1..))]
    workers: u64,

    /// Consecutive bundles a worker exports on one replay state, so the
    /// state a bundle read carries over to the next one.
    #[arg(long, default_value = "4")]
    run_bundles: u64,

    /// Blocks fetched upstream in one batch; must divide 1000.
    #[arg(long, default_value = "50")]
    chunk: u64,

    /// Blocks replayed on one cached state before reading the archive
    /// afresh, across the bundles of a run; a multiple of --chunk. Larger
    /// saves reads, costs memory.
    #[arg(long, default_value = "4000")]
    rebase_every: u64,

    /// Also compare the replayed state changes with the archive (slow).
    #[arg(long, default_value = "false")]
    check_state: bool,
}

impl Command {
    fn chain_id(&self) -> String {
        self.chain_id
            .clone()
            .unwrap_or_else(|| self.chain.chain_id().to_string())
    }

    /// The first and last bundle asked for.
    fn bundles(&self) -> Result<(u64, u64)> {
        // Bundle 0 is the genesis block alone.
        let bundle = |height: u64| -> Result<u64> {
            Ok(if height == 0 {
                0
            } else {
                (height - 1) / BUNDLE_SIZE + 1
            })
        };
        let first = match (self.from_bundle, self.from_block_height) {
            (Some(id), _) => id,
            (None, Some(height)) => bundle(height)?,
            (None, None) => unreachable!("clap requires a start"),
        };
        let last = match (self.end_bundle, self.end_block_height) {
            (Some(id), _) => id,
            (None, Some(height)) => bundle(height)?,
            (None, None) => unreachable!("clap requires an end"),
        };
        ensure!(first <= last, "the start is after the end");
        Ok((first, last))
    }

    /// The upload target of `resources`.
    async fn s3_target(&self, resources: &BTreeSet<Resource>) -> Result<S3Target> {
        for resource in resources {
            let (bucket, flag) = match resource {
                Resource::Validation | Resource::BlockFile => {
                    (&self.target_chain_table_bucket, "chain-table")
                }
                Resource::Header | Resource::StateDiff => (&self.target_nodex_bucket, "nodex"),
            };
            ensure!(
                bucket.is_some(),
                "exporting the {resource} bundle needs --target-{flag}-bucket"
            );
        }
        let chain_id = self.chain_id();
        for segment in std::iter::once(&chain_id).chain(&self.version) {
            ensure!(
                !segment.is_empty() && !segment.contains('/') && segment != "." && segment != "..",
                "--chain-id and --version must be single S3 key segments"
            );
        }
        ensure!(
            (1..=128).contains(&self.s3_concurrency),
            "--s3-concurrency must be between 1 and 128"
        );
        ensure!(
            self.statediff_part_size >= 5 << 20,
            "--statediff-part-size must be at least 5 MiB"
        );
        ensure!(self.gzip_level <= 9, "--gzip-level must be between 0 and 9");
        let config = aws_config::from_env()
            .region(aws_config::Region::new(self.region.clone()))
            .timeout_config(
                aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                    .operation_attempt_timeout(Duration::from_secs(600))
                    .build(),
            )
            .load()
            .await;
        let mut s3_config = aws_sdk_s3::config::Builder::from(&config);
        if let Some(endpoint) = &self.s3_endpoint {
            s3_config = s3_config.endpoint_url(endpoint).force_path_style(true);
        }
        let prefix = match &self.version {
            Some(version) => format!("{chain_id}/{version}"),
            None => chain_id,
        };
        Ok(S3Target {
            client: aws_sdk_s3::Client::from_conf(s3_config.build()),
            prefix,
            chain_table_bucket: self.target_chain_table_bucket.clone(),
            nodex_bucket: self.target_nodex_bucket.clone(),
            part_size: self.statediff_part_size,
            concurrency: self.s3_concurrency,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        leafage_evm_storage::set_inverted_block_encoding(self.inverted_block_encoding);
        std::env::set_var(
            "ARCHIVE_CODE_CACHE_MB",
            self.archive_code_cache_mb.to_string(),
        );
        let (from_bundle, to_bundle) = self.bundles()?;
        let resources: BTreeSet<Resource> = self.resources.iter().copied().collect();
        let s3 = self.s3_target(&resources).await?;
        // A typo'd path would otherwise create an empty DB.
        if !self.db_path.join("CURRENT").exists() {
            bail!("no RocksDB database found at {:?}", self.db_path);
        }
        info!(
            target: "export",
            chain = %self.chain,
            from_bundle,
            to_bundle,
            first_block = bundle_range(from_bundle).0,
            last_block = bundle_range(to_bundle).1,
            "exporting"
        );

        let db = MultiStorage::open(
            self.db_path.as_path(),
            self.db_cache,
            StorageKind::Rocksdb,
            true,
            false,
            self.archive_zstd_compression,
        )?;
        let tree = Arc::new(StateTree::new(
            db,
            StateTreeConfig::new(128, 10_000, 100_000, 10_000, true),
        )?);
        export_bundles(
            tree,
            self.chain,
            ExportConfig {
                block_rpc: self.block_rpc.clone(),
                from_bundle,
                to_bundle,
                resources,
                s3: Arc::new(s3),
                gzip_level: self.gzip_level,
                overwrite: self.overwrite,
                workers: self.workers as usize,
                run_bundles: self.run_bundles,
                chunk: self.chunk,
                rebase_every: self.rebase_every,
                check_state: self.check_state,
            },
        )
        .await
    }
}
