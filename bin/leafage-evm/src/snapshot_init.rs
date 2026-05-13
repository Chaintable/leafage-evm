use anyhow::{Context, Result};
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::Client;
use clap::Parser;
use futures::stream::{FuturesUnordered, StreamExt};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
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

// v3 archive 模式默认值,对齐 dev_v3.md §6.5。
const DEFAULT_ARCHIVE_CONCURRENCY: usize = 4;
const DEFAULT_OUTER_RETRY_ATTEMPTS: usize = 3;
const OUTER_RETRY_INITIAL_DELAY_MS: u64 = 1_000;
const OUTER_RETRY_MAX_DELAY_MS: u64 = 16_000;
const SHA256_READ_BUF_BYTES: usize = 1 << 20; // 1 MiB

const COMPRESSION_ZSTD: &str = "zstd";
const COMPRESSION_NONE: &str = "none";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct SnapshotConfig {
    pub endpoint: String,
    pub bucket: String,
    pub namespace: String,
    pub component: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub access_key_id: String,
    #[serde(default)]
    pub secret_access_key: String,
    #[serde(default)]
    pub concurrency: usize,

    // v3 archive 模式参数。manifest 决定走哪条路径;以下字段仅 archive 模式生效。
    /// archive 临时归档落盘根目录。留空 → <db_path>/.scratch
    #[serde(default)]
    pub scratch_dir: String,
    /// archive 级 worker pool 路数,默认 4。
    #[serde(default)]
    pub archive_concurrency: usize,
    /// 单 archive 外层重试次数,默认 3(SDK 内层 retry 之外)。
    #[serde(default)]
    pub retry_outer_attempts: usize,
}

#[derive(Debug, Deserialize, Clone)]
struct ArchiveEntry {
    name: String,
    #[serde(default)]
    size_bytes: i64,
    #[serde(default)]
    uncompressed_size_bytes: i64,
    sha256: String,
    #[serde(default)]
    file_count: i64,
}

#[derive(Debug, Deserialize)]
struct R2Manifest {
    schema_version: u32,
    format: String,
    #[serde(default)]
    compression: String,
    #[serde(default)]
    archive_size_bytes: i64,
    #[serde(default)]
    archives_prefix: String,
    #[serde(default)]
    archives: Vec<ArchiveEntry>,
    #[serde(default)]
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

struct Initializer {
    cfg: SnapshotConfig,
    db_path: PathBuf,
    s3_client: Client,
    concurrency: usize,
    archive_concurrency: usize,
    outer_retry_attempts: usize,
    scratch_dir: PathBuf,
}

impl Initializer {
    fn new(cfg: SnapshotConfig, db_path: PathBuf, scratch_override: Option<PathBuf>) -> Result<Self> {
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
        let archive_concurrency = if cfg.archive_concurrency == 0 {
            DEFAULT_ARCHIVE_CONCURRENCY
        } else {
            cfg.archive_concurrency
        };
        let outer_retry_attempts = if cfg.retry_outer_attempts == 0 {
            DEFAULT_OUTER_RETRY_ATTEMPTS
        } else {
            cfg.retry_outer_attempts
        };

        // scratch 解析优先级:CLI 覆盖 → 配置字段 → 默认 <db_path>/.scratch
        let scratch_dir = scratch_override
            .or_else(|| {
                if cfg.scratch_dir.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(&cfg.scratch_dir))
                }
            })
            .unwrap_or_else(|| db_path.join(".scratch"));

        Ok(Self {
            cfg,
            db_path,
            s3_client,
            concurrency,
            archive_concurrency,
            outer_retry_attempts,
            scratch_dir,
        })
    }

    async fn restore(&self) -> Result<()> {
        let chain_prefix = format!("{}/{}", self.cfg.namespace, self.cfg.component);
        let latest_key = format!("{}/latest.json", chain_prefix);

        info!(
            target: "snapshot_init",
            "fetching latest manifest bucket={} key={}",
            self.cfg.bucket, latest_key,
        );
        let manifest = self
            .fetch_latest_with_retry(&latest_key)
            .await
            .context("fetch latest.json")?;
        info!(
            target: "snapshot_init",
            "manifest schema_version={} format={} snap_id={} file_count={} total_size_bytes={} created_at={}",
            manifest.schema_version,
            manifest.format,
            manifest.snap_id,
            manifest.file_count,
            manifest.total_size_bytes,
            manifest.created_at,
        );

        // v2 schema 仅支持 per-file;v3 schema 同时支持 per-file 与 archive。
        match manifest.schema_version {
            2 => {
                if manifest.format != "per-file" {
                    anyhow::bail!(
                        "schema_version=2 only supports format=per-file, got {}",
                        manifest.format
                    );
                }
            }
            3 => {}
            other => anyhow::bail!("unsupported manifest.schema_version: {}", other),
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

        fs::create_dir_all(&self.db_path)
            .await
            .with_context(|| format!("mkdir {:?}", self.db_path))?;

        match manifest.format.as_str() {
            "per-file" => self.restore_per_file(&manifest, &chain_prefix).await,
            "archive" => self.restore_archive(&manifest, &chain_prefix).await,
            other => anyhow::bail!("unsupported manifest.format: {}", other),
        }
    }

    // === per-file 路径(v1/v2 兼容,v3 schema 下 format=per-file 也走这里)===

    async fn restore_per_file(&self, manifest: &R2Manifest, chain_prefix: &str) -> Result<()> {
        let expected_prefix_root = format!("{}/snapshots/", chain_prefix);
        if !manifest.files_prefix.starts_with(&expected_prefix_root) {
            anyhow::bail!(
                "manifest.files_prefix does not start with {}: got {}",
                expected_prefix_root,
                manifest.files_prefix
            );
        }

        info!(
            target: "snapshot_init",
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

        info!(
            target: "snapshot_init",
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
            target: "snapshot_init",
            "snapshot restore complete (per-file): {} files written",
            downloaded,
        );
        Ok(())
    }

    // === archive 路径(v3,format=archive)===

    async fn restore_archive(&self, manifest: &R2Manifest, chain_prefix: &str) -> Result<()> {
        let expected_prefix_root = format!("{}/snapshots/", chain_prefix);
        if !manifest.archives_prefix.starts_with(&expected_prefix_root) {
            anyhow::bail!(
                "manifest.archives_prefix does not start with {}: got {}",
                expected_prefix_root,
                manifest.archives_prefix
            );
        }
        if manifest.archives.is_empty() {
            anyhow::bail!("manifest.archives is empty");
        }
        match manifest.compression.as_str() {
            COMPRESSION_ZSTD | COMPRESSION_NONE => {}
            other => anyhow::bail!("unsupported manifest.compression: {}", other),
        }
        // 累计 file_count 校验
        let archives_file_total: i64 = manifest.archives.iter().map(|a| a.file_count).sum();
        if archives_file_total != manifest.file_count {
            anyhow::bail!(
                "sum(archives[].file_count)={} != manifest.file_count={}",
                archives_file_total,
                manifest.file_count
            );
        }

        // scratch 启动期清空 + 重建(本期假设环境干净;defer 任务末尾再清一次)。
        if self.scratch_dir.exists() {
            fs::remove_dir_all(&self.scratch_dir)
                .await
                .with_context(|| format!("rm -rf {:?}", self.scratch_dir))?;
        }
        fs::create_dir_all(&self.scratch_dir)
            .await
            .with_context(|| format!("mkdir {:?}", self.scratch_dir))?;

        info!(
            target: "snapshot_init",
            "archive restore start archives={} compression={} archive_concurrency={} scratch_dir={:?}",
            manifest.archives.len(),
            manifest.compression,
            self.archive_concurrency,
            self.scratch_dir,
        );

        let archives_prefix = manifest.archives_prefix.clone();
        let compression = manifest.compression.clone();

        // 镜像 r2-pusher 的 worker pool:FuturesUnordered + 滑动窗口。
        // 任一 worker 错 → result? 短路返回 → in_flight 集合 drop → 余下 future 自动取消(SDK GET 中断由 hyper 自身处理)。
        let mut iter = manifest.archives.iter().cloned().enumerate();
        let mut in_flight = FuturesUnordered::new();
        for _ in 0..self.archive_concurrency {
            if let Some((idx, entry)) = iter.next() {
                in_flight.push(self.run_one_archive(
                    idx,
                    entry,
                    archives_prefix.clone(),
                    compression.clone(),
                ));
            }
        }
        while let Some(result) = in_flight.next().await {
            result?;
            if let Some((idx, entry)) = iter.next() {
                in_flight.push(self.run_one_archive(
                    idx,
                    entry,
                    archives_prefix.clone(),
                    compression.clone(),
                ));
            }
        }

        // 整任务成功才清 scratch;失败保留现场供排障。
        if let Err(err) = fs::remove_dir_all(&self.scratch_dir).await {
            warn!(
                target: "snapshot_init",
                "scratch cleanup failed (continuing): {:#}",
                err,
            );
        }

        info!(
            target: "snapshot_init",
            "snapshot restore complete (archive): {} archives, {} files",
            manifest.archives.len(),
            manifest.file_count,
        );
        Ok(())
    }

    async fn run_one_archive(
        &self,
        idx: usize,
        entry: ArchiveEntry,
        archives_prefix: String,
        compression: String,
    ) -> Result<()> {
        let key = format!("{}{}", archives_prefix, entry.name);
        let local_path = self.scratch_dir.join(&entry.name);

        let mut last_err: Option<anyhow::Error> = None;
        let mut delay_ms = OUTER_RETRY_INITIAL_DELAY_MS;
        for attempt in 1..=self.outer_retry_attempts {
            match self
                .try_one_archive(&key, &entry, &compression, &local_path)
                .await
            {
                Ok(()) => {
                    info!(
                        target: "snapshot_init",
                        "archive done index={} name={} size_bytes={} files={} attempt={}",
                        idx,
                        entry.name,
                        entry.size_bytes,
                        entry.file_count,
                        attempt,
                    );
                    return Ok(());
                }
                Err(err) => {
                    warn!(
                        target: "snapshot_init",
                        "archive {} attempt {}/{} failed: {:#}",
                        entry.name, attempt, self.outer_retry_attempts, err,
                    );
                    last_err = Some(err);
                    // 删本地残骸,下一次 attempt 重新下载
                    let _ = fs::remove_file(&local_path).await;
                    if attempt < self.outer_retry_attempts {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        delay_ms = delay_ms.saturating_mul(4).min(OUTER_RETRY_MAX_DELAY_MS);
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("unknown error"))).with_context(|| {
            format!(
                "archive {} after {} attempts",
                entry.name, self.outer_retry_attempts
            )
        })
    }

    async fn try_one_archive(
        &self,
        key: &str,
        entry: &ArchiveEntry,
        compression: &str,
        local_path: &Path,
    ) -> Result<()> {
        // 1. 整文件下载到 scratch(单 SDK 调用,流式写盘,不进内存)
        self.download_to_file(key, local_path).await?;

        // 2. 整文件 sha256 校验(对 R2 对象内容字节,与 manifest.archives[i].sha256 比较)
        let expected = entry.sha256.clone();
        let local_path_owned = local_path.to_path_buf();
        let actual = tokio::task::spawn_blocking(move || hash_file_sha256(&local_path_owned))
            .await
            .context("sha256 task")??;
        if actual.eq_ignore_ascii_case(&expected) {
            // ok
        } else {
            anyhow::bail!(
                "sha256 mismatch for {}: expected {}, got {}",
                entry.name,
                expected,
                actual
            );
        }

        // 3. 流式解压 → 落 db_path(sync zstd + sync tar 在 spawn_blocking 内执行)
        let compression_owned = compression.to_string();
        let local_for_extract = local_path.to_path_buf();
        let db_path_owned = self.db_path.clone();
        let expected_files = entry.file_count;
        tokio::task::spawn_blocking(move || {
            decompress_archive(&local_for_extract, &compression_owned, &db_path_owned, expected_files)
        })
        .await
        .context("decompress task")??;

        // 4. 立即删本地 scratch 文件(限制峰值盘 = concurrency × archive_size)
        fs::remove_file(local_path)
            .await
            .with_context(|| format!("rm {:?}", local_path))?;

        Ok(())
    }

    /// 流式下载单对象到本地文件,不进内存。
    async fn download_to_file(&self, key: &str, local_path: &Path) -> Result<()> {
        // SDK 内层 retryer 兜底瞬时网络;持续失败抛错由外层 wrapper 处理。
        let resp = self
            .s3_client
            .get_object()
            .bucket(&self.cfg.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("get_object {}/{}", self.cfg.bucket, key))?;
        let mut body = resp.body;
        let mut file = fs::File::create(local_path)
            .await
            .with_context(|| format!("create {:?}", local_path))?;
        while let Some(chunk) = body.try_next().await? {
            file.write_all(&chunk)
                .await
                .with_context(|| format!("write {:?}", local_path))?;
        }
        file.flush().await?;
        Ok(())
    }

    // === per-file 模式下使用的旧实现(原样保留)===

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
                        target: "snapshot_init",
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
                        target: "snapshot_init",
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

/// 同步 sha256:1 MiB 块流式读取,返回小写 hex。
fn hash_file_sha256(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("open {:?}", path))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; SHA256_READ_BUF_BYTES];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 同步流式解压:文件 → zstd 解码(可选)→ tar 解析 → 落 db_path。
///
/// 在 spawn_blocking 内执行;tar / zstd crate 都是 sync API。
/// 单 archive 解压过程内存占用 KB 级(逐 entry 流式)。
fn decompress_archive(local: &Path, compression: &str, db_path: &Path, expected_files: i64) -> Result<()> {
    let file = std::fs::File::open(local).with_context(|| format!("open {:?}", local))?;
    let reader: Box<dyn Read + Send> = match compression {
        COMPRESSION_ZSTD => Box::new(
            zstd::stream::read::Decoder::new(file).context("zstd decoder")?,
        ),
        COMPRESSION_NONE => Box::new(file),
        other => anyhow::bail!("unsupported compression {}", other),
    };
    let mut archive = tar::Archive::new(reader);
    // 沿用 tar 默认权限/owner 行为,但不允许 entry 路径越界(防 path traversal)。
    archive.set_overwrite(true);
    archive.set_preserve_mtime(false);
    archive.set_preserve_permissions(false);

    let mut count: i64 = 0;
    for entry in archive.entries().context("tar entries")? {
        let mut entry = entry.context("tar entry")?;
        let entry_path = entry
            .path()
            .context("tar entry path")?
            .into_owned();
        // 拒绝绝对路径与 ".." 越界
        for comp in entry_path.components() {
            use std::path::Component;
            match comp {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    anyhow::bail!("tar entry path escapes db_path: {:?}", entry_path);
                }
            }
        }
        let dst = db_path.join(&entry_path);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {:?}", parent))?;
        }
        // 只处理常规文件(tar header type=0/'\0'/'0');目录由 mkdir 兜底,符号链接/特殊文件 skip。
        match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => {}
            tar::EntryType::Directory => continue,
            other => {
                warn!(
                    target: "snapshot_init",
                    "skip non-regular tar entry path={:?} type={:?}",
                    entry_path, other,
                );
                continue;
            }
        }
        entry
            .unpack(&dst)
            .with_context(|| format!("unpack {:?}", dst))?;
        count += 1;
    }
    if count != expected_files {
        anyhow::bail!(
            "decompress: extracted {} files but manifest expected {}",
            count,
            expected_files
        );
    }
    Ok(())
}

/// `leafage-evm snapshot-init` command
#[derive(Debug, Parser)]
pub(crate) struct Command {
    /// The path to the database directory to populate.
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// The R2 snapshot config path or inline JSON.
    #[arg(long, value_parser = parse_snapshot_config, value_name = "SNAPSHOT_CONFIG_PATH")]
    snapshot_config: SnapshotConfig,

    /// archive 模式 scratch 临时归档落盘根目录。覆盖配置中的 scratch_dir;留空 → <db_path>/.scratch。
    #[arg(long, value_name = "PATH")]
    scratch_dir: Option<PathBuf>,
}

fn parse_snapshot_config(arg: &str) -> Result<SnapshotConfig> {
    let cfg: SnapshotConfig = if std::path::Path::new(arg).exists() {
        let file = std::fs::File::open(arg)?;
        serde_json::from_reader(file)?
    } else {
        serde_json::from_str(arg)?
    };
    Ok(cfg)
}

impl Command {
    pub(crate) async fn run(&mut self) -> Result<()> {
        info!(
            target: "snapshot_init",
            "starting snapshot-init db_path={:?} scratch_override={:?}",
            self.db_path, self.scratch_dir,
        );
        let initializer = Initializer::new(
            self.snapshot_config.clone(),
            self.db_path.clone(),
            self.scratch_dir.clone(),
        )?;
        initializer.restore().await?;
        info!(target: "snapshot_init", "snapshot-init finished");
        Ok(())
    }
}
