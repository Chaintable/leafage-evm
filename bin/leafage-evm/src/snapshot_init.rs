use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::{FuturesUnordered, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::fs;
use tokio_util::io::{StreamReader, SyncIoBridge};
use tracing::{info, warn};

const LATEST_RETRY_INITIAL_DELAY_MS: u64 = 1_000;
const LATEST_RETRY_MAX_DELAY_MS: u64 = 60_000;
const LATEST_RETRY_MAX_ATTEMPTS: usize = 6;

// 当前调试阶段固定支持的 manifest schema 版本。
const SUPPORTED_SCHEMA_VERSION: u32 = 1;

// archive 模式默认并发,可由配置覆盖。
const DEFAULT_ARCHIVE_CONCURRENCY: usize = 10;

// 单 archive 外层重试默认 3 次,指数退避 1s → 4s。
// HTTP 客户端内层重试兜底瞬时网络抖动,外层兜底持续故障。
const DEFAULT_OUTER_RETRY_ATTEMPTS: usize = 3;
const OUTER_RETRY_INITIAL_DELAY_MS: u64 = 1_000;
const OUTER_RETRY_MAX_DELAY_MS: u64 = 16_000;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct SnapshotConfig {
    /// 公开桶域名(base URL),形如 https://snapshots.chaintable.<TLD>。
    /// 公网无鉴权 GET,不再需要 endpoint / bucket / region / ak / sk。
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub gateway_base_url: String,
    #[serde(default)]
    pub chain_id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub is_archive: bool,
    pub namespace: String,
    pub component: String,

    /// archive 级 worker pool 路数,默认 10。
    #[serde(default)]
    pub archive_concurrency: usize,

    /// 单 archive 外层重试次数,默认 3(HTTP 内层 retry 之外)。
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
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    file_count: i64,
    #[serde(default)]
    url: String,
}

#[derive(Debug, Deserialize)]
struct R2Manifest {
    schema_version: u32,
    #[serde(default)]
    compression: String,
    #[serde(default)]
    archive_size_bytes: i64,
    #[serde(default)]
    archives_prefix: String,
    #[serde(default)]
    archives: Vec<ArchiveEntry>,
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
    /// 公网 base URL,已去掉尾随 '/'。
    base_url: String,
    http: reqwest::Client,
    archive_concurrency: usize,
    outer_retry_attempts: usize,
}

#[derive(Debug)]
struct GlobalProgress {
    total_archives: usize,
    completed_archives: AtomicU64,
    total_bytes: u64,
    downloaded_bytes: Arc<AtomicU64>,
    finished: AtomicBool,
}

impl Initializer {
    fn new(cfg: SnapshotConfig, db_path: PathBuf) -> Result<Self> {
        if cfg.base_url.is_empty() && cfg.gateway_base_url.is_empty() {
            anyhow::bail!("snapshot_config.base_url or snapshot_config.gateway_base_url is required");
        }
        if !cfg.gateway_base_url.is_empty() {
            if cfg.chain_id.is_empty() {
                anyhow::bail!("snapshot_config.chain_id is required when gateway_base_url is set");
            }
            if cfg.version.is_empty() {
                anyhow::bail!("snapshot_config.version is required when gateway_base_url is set");
            }
        }
        if cfg.namespace.is_empty() {
            anyhow::bail!("snapshot_config.namespace is required");
        }
        if cfg.component.is_empty() {
            anyhow::bail!("snapshot_config.component is required");
        }

        let base_url = cfg.base_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .build()
            .context("build http client")?;

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

        Ok(Self {
            cfg,
            db_path,
            base_url,
            http,
            archive_concurrency,
            outer_retry_attempts,
        })
    }

    /// 把相对 key 拼成完整公网 URL。key 不以 '/' 开头。
    fn object_url(&self, key: &str) -> String {
        format!("{}/{}", self.base_url, key)
    }

    async fn restore(&self) -> Result<()> {
        let chain_prefix = format!("{}/{}", self.cfg.namespace, self.cfg.component);
        let latest_key = format!("{}/latest.json", chain_prefix);

        let manifest = if self.cfg.gateway_base_url.is_empty() {
            info!(
                target: "snapshot_init",
                "fetching latest manifest url={}",
                self.object_url(&latest_key),
            );
            self.fetch_latest_with_retry(&latest_key)
                .await
                .context("fetch latest.json")?
        } else {
            info!(
                target: "snapshot_init",
                "fetching latest manifest from gateway base={} chain_id={} version={} is_archive={}",
                self.cfg.gateway_base_url,
                self.cfg.chain_id,
                self.cfg.version,
                self.cfg.is_archive,
            );
            self.fetch_gateway_manifest_with_retry()
                .await
                .context("fetch gateway snapshot manifest")?
        };
        info!(
            target: "snapshot_init",
            "manifest schema_version={} snap_id={} file_count={} total_size_bytes={} created_at={}",
            manifest.schema_version,
            manifest.snap_id,
            manifest.file_count,
            manifest.total_size_bytes,
            manifest.created_at,
        );

        // 调试阶段固定只支持 schema_version=1;非 1 直接 bail。
        if manifest.schema_version != SUPPORTED_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported manifest.schema_version: {} (only {} supported)",
                manifest.schema_version,
                SUPPORTED_SCHEMA_VERSION
            );
        }
        if manifest.namespace != self.cfg.namespace || manifest.component != self.cfg.component {
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

        // snapshot-init 只允许在空目录执行,避免静默覆盖已有数据。
        self.ensure_db_path_empty().await?;

        self.restore_archive(&manifest, &chain_prefix).await
    }

    /// 确保 db_path 为空。目录本身允许存在,但只要有任何直接子项就报错。
    async fn ensure_db_path_empty(&self) -> Result<()> {
        let mut rd = fs::read_dir(&self.db_path)
            .await
            .with_context(|| format!("read_dir {:?}", self.db_path))?;
        if let Some(entry) = rd
            .next_entry()
            .await
            .with_context(|| format!("next_entry {:?}", self.db_path))?
        {
            anyhow::bail!(
                "db_path is not empty, refusing to overwrite existing data: {:?} (first entry: {:?})",
                self.db_path,
                entry.path()
            );
        }
        Ok(())
    }

    // === archive 恢复(schema_version=1 唯一路径)===
    // 流式管道:HTTP GET → bytes_stream → StreamReader → SyncIoBridge → tar::Archive → 直接 unpack 到 db_path。
    // 不落本地盘,不算 sha256。任一 archive 用尽重试 → 整任务退出。

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

        // 压缩由生产端 compression 配置决定,写在 manifest.compression(权威来源)。
        // 早判早错:不支持的算法在开始下载前就退出。整份 snapshot 同一压缩,解析一次即可。
        //   "zstd"        → 流式 zstd 解码后再 tar
        //   "none" / ""   → 原始字节直喂 tar
        //   其他          → 直接报错(目前只支持 zstd)
        let compressed = match manifest.compression.as_str() {
            "zstd" => true,
            "none" | "" => false,
            other => anyhow::bail!(
                "unsupported compression algorithm: {} (only zstd / none supported)",
                other
            ),
        };

        info!(
            target: "snapshot_init",
            "archive restore start archives={} archive_concurrency={} compression={}",
            manifest.archives.len(),
            self.archive_concurrency,
            if compressed { "zstd" } else { "none" },
        );

        let archives_prefix = manifest.archives_prefix.clone();
        let total_bytes = manifest
            .archives
            .iter()
            .map(|entry| u64::try_from(entry.size_bytes).unwrap_or(0))
            .sum();
        let progress = Arc::new(GlobalProgress {
            total_archives: manifest.archives.len(),
            completed_archives: AtomicU64::new(0),
            total_bytes,
            downloaded_bytes: Arc::new(AtomicU64::new(0)),
            finished: AtomicBool::new(false),
        });
        let progress_task = spawn_global_progress_logger(Arc::clone(&progress));

        // worker pool:FuturesUnordered + 滑动窗口。
        // 任一 worker 错 → result? 短路返回 → in_flight 集合 drop → 余下 future 自动取消。
        let mut iter = manifest.archives.iter().cloned().enumerate();
        let mut in_flight = FuturesUnordered::new();
        for _ in 0..self.archive_concurrency {
            if let Some((idx, entry)) = iter.next() {
                in_flight.push(self.extract_one_archive(
                    idx,
                    entry,
                    archives_prefix.clone(),
                    compressed,
                    Arc::clone(&progress),
                ));
            }
        }
        while let Some(result) = in_flight.next().await {
            result?;
            if let Some((idx, entry)) = iter.next() {
                in_flight.push(self.extract_one_archive(
                    idx,
                    entry,
                    archives_prefix.clone(),
                    compressed,
                    Arc::clone(&progress),
                ));
            }
        }
        progress.finished.store(true, Ordering::Relaxed);
        let _ = progress_task.await;

        info!(
            target: "snapshot_init",
            "snapshot restore complete (archive): {} archives, {} files",
            manifest.archives.len(),
            manifest.file_count,
        );
        Ok(())
    }

    /// 单 archive 流式解压:GET → StreamReader → SyncIoBridge → tar → unpack 直接落 db_path。
    /// 不下载到 scratch、不算 sha256;外层包 retry × N(指数退避 1s → 4s)。
    /// 任一 attempt 失败 → warn + 退避后下一次;N 次都失败 → 返回 Err 由调用方短路整轮。
    /// 失败时已落 db_path 的数据不主动清理(由调用方/运维决定)。
    async fn extract_one_archive(
        &self,
        idx: usize,
        entry: ArchiveEntry,
        archives_prefix: String,
        compressed: bool,
        progress: Arc<GlobalProgress>,
    ) -> Result<()> {
        let key = format!("{}{}", archives_prefix, entry.name);
        let url = entry.url.clone();
        info!(
            target: "snapshot_init",
            "archive start index={} name={} size_bytes={} files={} url_present={}",
            idx, entry.name, entry.size_bytes, entry.file_count, !url.is_empty(),
        );

        let mut last_err: Option<anyhow::Error> = None;
        let mut delay_ms = OUTER_RETRY_INITIAL_DELAY_MS;
        for attempt in 1..=self.outer_retry_attempts {
            // Per-attempt counter: only merged into global on success, so retries
            // never inflate the progress percentage above 100%.
            let attempt_bytes = Arc::new(AtomicU64::new(0));
            match self
                .try_extract_one_archive(
                    &key,
                    if url.is_empty() { None } else { Some(url.as_str()) },
                    compressed,
                    entry.size_bytes,
                    Arc::clone(&attempt_bytes),
                )
                .await
            {
                Ok(()) => {
                    info!(
                        target: "snapshot_init",
                        "archive done index={} name={} attempt={}",
                        idx, entry.name, attempt,
                    );
                    progress.downloaded_bytes.fetch_add(
                        attempt_bytes.load(Ordering::Relaxed),
                        Ordering::Relaxed,
                    );
                    progress
                        .completed_archives
                        .fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(err) => {
                    warn!(
                        target: "snapshot_init",
                        "archive {} attempt {}/{} failed: {:#}",
                        entry.name, attempt, self.outer_retry_attempts, err,
                    );
                    last_err = Some(err);
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

    /// 单次尝试:完整跑一遍流式管道(GET → StreamReader → SyncIoBridge → tar::Archive::unpack)。
    /// 中途任何错(GET 失败 / 非 2xx / tar 解析 / 写盘)直接抛出,由外层 wrapper 决定是否重试。
    /// bytes_counter 是本次 attempt 的独立计数器,由调用方决定是否合并进全局进度。
    async fn try_extract_one_archive(
        &self,
        key: &str,
        archive_url: Option<&str>,
        compressed: bool,
        archive_size_bytes: i64,
        bytes_counter: Arc<AtomicU64>,
    ) -> Result<()> {
        let url = archive_url
            .map(str::to_string)
            .unwrap_or_else(|| self.object_url(key));
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("get {}", url))?
            .error_for_status()
            .with_context(|| format!("get {} returned error status", url))?;

        let total_bytes = u64::try_from(archive_size_bytes).unwrap_or(0);

        // bytes_stream → io::Error → StreamReader(AsyncRead)→ SyncIoBridge(sync Read)。
        // Box::pin 保证 StreamReader: Unpin(SyncIoBridge 的同步 Read impl 要求)。
        let stream = Box::pin(
            resp.bytes_stream()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)),
        );
        let async_read = StreamReader::new(stream);
        let db_path = self.db_path.clone();
        // SyncIoBridge 需要在 tokio runtime context 内构造(捕获 Handle),再 move 进 spawn_blocking。
        let sync_read = SyncIoBridge::new(CountedAsyncRead::new(
            async_read,
            bytes_counter,
            total_bytes,
        ));
        tokio::task::spawn_blocking(move || extract_tar(sync_read, &db_path, compressed))
            .await
            .context("extract task")?
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
        let url = self.object_url(key);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("get {}", url))?
            .error_for_status()
            .with_context(|| format!("get {} returned error status", url))?;
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("read body {}", url))?;
        Ok(bytes.to_vec())
    }

    async fn fetch_gateway_manifest_with_retry(&self) -> Result<R2Manifest> {
        let mut delay_ms = LATEST_RETRY_INITIAL_DELAY_MS;
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=LATEST_RETRY_MAX_ATTEMPTS {
            match self.fetch_gateway_manifest().await {
                Ok(manifest) => return Ok(manifest),
                Err(err) => {
                    warn!(
                        target: "snapshot_init",
                        "gateway snapshot manifest attempt {}/{} failed: {:#}",
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
            .context("gateway snapshot manifest not available after retries")
    }

    async fn fetch_gateway_manifest(&self) -> Result<R2Manifest> {
        let base = self.cfg.gateway_base_url.trim_end_matches('/');
        let url = format!(
            "{}/v1/snapshot?chain_id={}&version={}&is_archive={}",
            base, self.cfg.chain_id, self.cfg.version, self.cfg.is_archive
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("get {}", url))?
            .error_for_status()
            .with_context(|| format!("get {} returned error status", url))?;
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("read body {}", url))?;
        serde_json::from_slice(&bytes).context("parse gateway snapshot manifest")
    }
}

/// 同步流式解包:reader → (可选 zstd 流式解码) → tar entries → 直接落 db_path。
/// 在 spawn_blocking 内执行。compressed=true 时先套一层流式 zstd 解码器,
/// 仍是边读边解边写、不缓存整片,内存占用 KB~MB 级。
fn extract_tar<R: std::io::Read>(reader: R, db_path: &Path, compressed: bool) -> Result<()> {
    if compressed {
        let decoder =
            zstd::stream::read::Decoder::new(reader).context("init zstd stream decoder")?;
        unpack_tar(decoder, db_path)
    } else {
        unpack_tar(reader, db_path)
    }
}

struct CountedAsyncRead<R> {
    inner: R,
    global_count: Arc<AtomicU64>,
    total_bytes: u64,
    local_count: u64,
}

impl<R> CountedAsyncRead<R> {
    fn new(inner: R, global_count: Arc<AtomicU64>, total_bytes: u64) -> Self {
        Self {
            inner,
            global_count,
            total_bytes,
            local_count: 0,
        }
    }
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for CountedAsyncRead<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let filled_before = buf.filled().len();
        match std::pin::Pin::new(&mut self.inner).poll_read(cx, buf) {
            std::task::Poll::Ready(Ok(())) => {
                let filled_after = buf.filled().len();
                if filled_after > filled_before {
                    let delta = (filled_after - filled_before) as u64;
                    let remaining = self.total_bytes.saturating_sub(self.local_count);
                    let accounted = delta.min(remaining);
                    self.local_count = self.local_count.saturating_add(accounted);
                    if accounted > 0 {
                        self.global_count.fetch_add(accounted, Ordering::Relaxed);
                    }
                }
                std::task::Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", UNITS[unit])
}

fn eta_secs(elapsed: Duration, done: u64, total: u64) -> Option<u64> {
    if done == 0 || total == 0 || done >= total {
        return None;
    }
    let elapsed_secs = elapsed.as_secs_f64();
    if elapsed_secs <= 0.0 {
        return None;
    }
    let rate = done as f64 / elapsed_secs;
    if rate <= 0.0 {
        return None;
    }
    let remain = (total - done) as f64 / rate;
    Some(remain.ceil().max(0.0) as u64)
}

fn spawn_global_progress_logger(progress: Arc<GlobalProgress>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let started_at = tokio::time::Instant::now();
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            if progress.finished.load(Ordering::Relaxed) {
                break;
            }
            let done = progress.downloaded_bytes.load(Ordering::Relaxed);
            let total_bytes = progress.total_bytes;
            let completed = progress.completed_archives.load(Ordering::Relaxed);
            let active = progress.total_archives.saturating_sub(completed as usize);
            let percent = if total_bytes == 0 {
                0.0
            } else {
                done as f64 * 100.0 / total_bytes as f64
            };
            let eta = eta_secs(started_at.elapsed(), done, total_bytes)
                .map(|s| format!("{s}s"))
                .unwrap_or_else(|| "n/a".to_string());
            info!(
                target: "snapshot_init",
                "snapshot progress {:.1}% ({}/{}) archives_completed={}/{} active={} eta={}",
                percent,
                format_bytes(done),
                format_bytes(total_bytes),
                completed,
                progress.total_archives,
                active,
                eta,
            );
        }
    })
}

/// 逐 entry 流式解 tar 落盘(防越权:拒绝绝对路径 / ".." / 设备等特殊文件)。
fn unpack_tar<R: std::io::Read>(reader: R, db_path: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_overwrite(true);
    archive.set_preserve_mtime(false);
    archive.set_preserve_permissions(false);

    for entry in archive.entries().context("tar entries")? {
        let mut entry = entry.context("tar entry")?;
        let entry_path = entry.path().context("tar entry path")?.into_owned();
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
            std::fs::create_dir_all(parent).with_context(|| format!("mkdir {:?}", parent))?;
        }
        // 只处理常规文件;目录由 mkdir 兜底,符号链接/特殊文件 skip。
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
            "starting snapshot-init db_path={:?}",
            self.db_path,
        );
        let initializer = Initializer::new(self.snapshot_config.clone(), self.db_path.clone())?;
        initializer.restore().await?;
        info!(target: "snapshot_init", "snapshot-init finished");
        Ok(())
    }
}
