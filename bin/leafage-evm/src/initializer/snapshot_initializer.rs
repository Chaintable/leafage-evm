use crate::utils::SnapshotConfig;
use anyhow::{Context, Result};
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::Client;
use futures::stream::{FuturesUnordered, StreamExt};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

const DEFAULT_CONCURRENCY: usize = 32;
const LATEST_RETRY_INITIAL_DELAY_MS: u64 = 1_000;
const LATEST_RETRY_MAX_DELAY_MS: u64 = 60_000;
const LATEST_RETRY_MAX_ATTEMPTS: usize = 6;
const PER_FILE_MAX_ATTEMPTS: usize = 3;

#[derive(Debug, Deserialize)]
struct R2Manifest {
    schema_version: u32,
    format: String,
    files_prefix: String,
    namespace: String,
    component: String,
    snap_id: String,
    #[serde(default)]
    file_count: i64,
    #[serde(default)]
    total_size_bytes: i64,
    #[serde(default)]
    created_at: String,
}

pub struct Initializer {
    cfg: SnapshotConfig,
    db_path: PathBuf,
    s3_client: Client,
    concurrency: usize,
}

impl Initializer {
    pub fn new(cfg: SnapshotConfig, db_path: PathBuf) -> Result<Self> {
        if cfg.endpoint.is_empty() {
            anyhow::bail!("snapshot_config.endpoint is required");
        }
        if cfg.bucket.is_empty() {
            anyhow::bail!("snapshot_config.bucket is required");
        }
        if cfg.namespace.is_empty() {
            anyhow::bail!("snapshot_config.namespace is required");
        }
        if cfg.component.is_empty() {
            anyhow::bail!("snapshot_config.component is required");
        }

        // env 优先,配置 fallback
        let access_key = std::env::var("R2_ACCESS_KEY_ID")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| cfg.access_key_id.clone());
        let secret_key = std::env::var("R2_SECRET_ACCESS_KEY")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| cfg.secret_access_key.clone());
        if access_key.is_empty() || secret_key.is_empty() {
            anyhow::bail!(
                "R2 credentials missing: set env R2_ACCESS_KEY_ID/R2_SECRET_ACCESS_KEY \
                 or fill snapshot_config.access_key_id/secret_access_key"
            );
        }

        let region = if cfg.region.is_empty() {
            "auto".to_string()
        } else {
            cfg.region.clone()
        };
        let creds = Credentials::new(access_key, secret_key, None, None, "r2-static");
        let s3_config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region))
            .endpoint_url(&cfg.endpoint)
            .credentials_provider(creds)
            .force_path_style(true)
            .build();
        let s3_client = Client::from_conf(s3_config);

        let concurrency = if cfg.concurrency == 0 {
            DEFAULT_CONCURRENCY
        } else {
            cfg.concurrency
        };

        Ok(Self {
            cfg,
            db_path,
            s3_client,
            concurrency,
        })
    }

    pub async fn restore(&self) -> Result<()> {
        let chain_prefix = format!("{}/{}", self.cfg.namespace, self.cfg.component);
        let latest_key = format!("{}/latest.json", chain_prefix);

        info!(
            target: "snapshot_initializer",
            "fetching latest manifest bucket={} key={}",
            self.cfg.bucket, latest_key,
        );
        let manifest = self
            .fetch_latest_with_retry(&latest_key)
            .await
            .context("fetch latest.json")?;
        info!(
            target: "snapshot_initializer",
            "manifest snap_id={} file_count={} total_size_bytes={} created_at={}",
            manifest.snap_id, manifest.file_count, manifest.total_size_bytes, manifest.created_at,
        );

        if manifest.schema_version != 2 {
            anyhow::bail!(
                "unsupported manifest.schema_version: {}",
                manifest.schema_version
            );
        }
        if manifest.format != "per-file" {
            anyhow::bail!("unsupported manifest.format: {}", manifest.format);
        }
        if manifest.namespace != self.cfg.namespace
            || manifest.component != self.cfg.component
        {
            anyhow::bail!(
                "manifest namespace/component mismatch: manifest={}/{} cfg={}/{}",
                manifest.namespace,
                manifest.component,
                self.cfg.namespace,
                self.cfg.component
            );
        }
        let expected_prefix_root = format!("{}/snapshots/", chain_prefix);
        if !manifest.files_prefix.starts_with(&expected_prefix_root) {
            anyhow::bail!(
                "manifest.files_prefix does not start with {}: got {}",
                expected_prefix_root,
                manifest.files_prefix
            );
        }

        info!(
            target: "snapshot_initializer",
            "listing objects under {}",
            manifest.files_prefix,
        );
        let keys = self
            .list_files(&manifest.files_prefix)
            .await
            .context("list files")?;
        if (keys.len() as i64) != manifest.file_count {
            anyhow::bail!(
                "listed {} keys but manifest.file_count is {}",
                keys.len(),
                manifest.file_count
            );
        }

        fs::create_dir_all(&self.db_path)
            .await
            .with_context(|| format!("mkdir {:?}", self.db_path))?;

        info!(
            target: "snapshot_initializer",
            "downloading {} files into {:?} concurrency={}",
            keys.len(),
            self.db_path,
            self.concurrency,
        );
        let downloaded = self
            .download_files(keys, &manifest.files_prefix)
            .await
            .context("download files")?;
        if (downloaded as i64) != manifest.file_count {
            anyhow::bail!(
                "downloaded {} files but manifest.file_count is {}",
                downloaded,
                manifest.file_count
            );
        }
        info!(
            target: "snapshot_initializer",
            "snapshot restore complete: {} files written",
            downloaded,
        );
        Ok(())
    }

    async fn fetch_latest_with_retry(&self, key: &str) -> Result<R2Manifest> {
        let mut delay_ms = LATEST_RETRY_INITIAL_DELAY_MS;
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=LATEST_RETRY_MAX_ATTEMPTS {
            match self.fetch_object_bytes(key).await {
                Ok(bytes) => {
                    let m: R2Manifest =
                        serde_json::from_slice(&bytes).context("parse latest.json")?;
                    return Ok(m);
                }
                Err(err) => {
                    warn!(
                        target: "snapshot_initializer",
                        "latest.json fetch attempt {}/{} failed: {:#}",
                        attempt, LATEST_RETRY_MAX_ATTEMPTS, err,
                    );
                    last_err = Some(err);
                    if attempt < LATEST_RETRY_MAX_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        delay_ms = (delay_ms.saturating_mul(2)).min(LATEST_RETRY_MAX_DELAY_MS);
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("unknown error")))
            .context("latest.json not available after retries")
    }

    async fn fetch_object_bytes(&self, key: &str) -> Result<Vec<u8>> {
        let resp = self
            .s3_client
            .get_object()
            .bucket(&self.cfg.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("get_object {}/{}", self.cfg.bucket, key))?;
        let bytes = resp.body.collect().await?.into_bytes();
        Ok(bytes.to_vec())
    }

    async fn list_files(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self
                .s3_client
                .list_objects_v2()
                .bucket(&self.cfg.bucket)
                .prefix(prefix);
            if let Some(t) = token.as_deref() {
                req = req.continuation_token(t);
            }
            let resp = req
                .send()
                .await
                .with_context(|| format!("list_objects_v2 {}/{}", self.cfg.bucket, prefix))?;
            for obj in resp.contents() {
                if let Some(k) = obj.key() {
                    keys.push(k.to_string());
                }
            }
            if resp.is_truncated().unwrap_or(false) {
                match resp.next_continuation_token() {
                    Some(t) => token = Some(t.to_string()),
                    None => break,
                }
            } else {
                break;
            }
        }
        Ok(keys)
    }

    async fn download_files(&self, keys: Vec<String>, files_prefix: &str) -> Result<usize> {
        let mut downloaded: usize = 0;
        let mut iter = keys.into_iter();
        let mut in_flight: FuturesUnordered<_> = FuturesUnordered::new();
        for _ in 0..self.concurrency {
            if let Some(key) = iter.next() {
                in_flight.push(self.download_one(key, files_prefix.to_string()));
            }
        }
        while let Some(result) = in_flight.next().await {
            result?;
            downloaded += 1;
            if let Some(key) = iter.next() {
                in_flight.push(self.download_one(key, files_prefix.to_string()));
            }
        }
        Ok(downloaded)
    }

    async fn download_one(&self, key: String, files_prefix: String) -> Result<()> {
        let rel = key.strip_prefix(&files_prefix).ok_or_else(|| {
            anyhow::anyhow!(
                "key {} does not start with files_prefix {}",
                key,
                files_prefix
            )
        })?;
        let local_path = self.db_path.join(rel);
        if let Some(parent) = local_path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("mkdir {:?}", parent))?;
        }
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=PER_FILE_MAX_ATTEMPTS {
            match self.download_one_attempt(&key, &local_path).await {
                Ok(()) => return Ok(()),
                Err(err) => {
                    warn!(
                        target: "snapshot_initializer",
                        "download {} attempt {}/{} failed: {:#}",
                        key, attempt, PER_FILE_MAX_ATTEMPTS, err,
                    );
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("unknown error"))).with_context(|| {
            format!("download {} after {} attempts", key, PER_FILE_MAX_ATTEMPTS)
        })
    }

    async fn download_one_attempt(&self, key: &str, local_path: &Path) -> Result<()> {
        let resp = self
            .s3_client
            .get_object()
            .bucket(&self.cfg.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("get_object {}/{}", self.cfg.bucket, key))?;
        let bytes = resp.body.collect().await?.into_bytes();
        let mut file = fs::File::create(local_path)
            .await
            .with_context(|| format!("create {:?}", local_path))?;
        file.write_all(&bytes)
            .await
            .with_context(|| format!("write {:?}", local_path))?;
        file.flush().await?;
        Ok(())
    }
}
