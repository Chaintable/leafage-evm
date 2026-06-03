use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::{FuturesUnordered, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
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
const DEFAULT_ARCHIVE_CONCURRENCY: usize = 4;

// 单 archive 外层重试默认 3 次,指数退避 1s → 4s。
// HTTP 客户端内层重试兜底瞬时网络抖动,外层兜底持续故障。
const DEFAULT_OUTER_RETRY_ATTEMPTS: usize = 3;
const OUTER_RETRY_INITIAL_DELAY_MS: u64 = 1_000;
const OUTER_RETRY_MAX_DELAY_MS: u64 = 16_000;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct SnapshotConfig {
    /// 公开桶域名(base URL),形如 https://snapshots.chaintable.<TLD>。
    /// 公网无鉴权 GET,不再需要 endpoint / bucket / region / ak / sk。
    pub base_url: String,
    pub namespace: String,
    pub component: String,

    /// archive 级 worker pool 路数,默认 4。
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

impl Initializer {
    fn new(cfg: SnapshotConfig, db_path: PathBuf) -> Result<Self> {
        if cfg.base_url.is_empty() {
            anyhow::bail!("snapshot_config.base_url is required");
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

        info!(
            target: "snapshot_init",
            "fetching latest manifest url={}",
            self.object_url(&latest_key),
        );
        let manifest = self
            .fetch_latest_with_retry(&latest_key)
            .await
            .context("fetch latest.json")?;
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

        // 进 init 前清空 db_path 现有内容,避免上一次半套残留与本次 snapshot 混杂。
        // 目录本身保留(容器场景常见挂载点),只删直接子项。
        self.purge_db_path().await?;

        self.restore_archive(&manifest, &chain_prefix).await
    }

    /// 清空 db_path 下所有直接子项(目录本身保留,适配 bind-mount / 挂载点场景)。
    async fn purge_db_path(&self) -> Result<()> {
        let mut rd = fs::read_dir(&self.db_path)
            .await
            .with_context(|| format!("read_dir {:?}", self.db_path))?;
        let mut removed: usize = 0;
        while let Some(entry) = rd
            .next_entry()
            .await
            .with_context(|| format!("next_entry {:?}", self.db_path))?
        {
            let path = entry.path();
            let ft = entry
                .file_type()
                .await
                .with_context(|| format!("file_type {:?}", path))?;
            if ft.is_dir() {
                fs::remove_dir_all(&path)
                    .await
                    .with_context(|| format!("rm -rf {:?}", path))?;
            } else {
                fs::remove_file(&path)
                    .await
                    .with_context(|| format!("rm {:?}", path))?;
            }
            removed += 1;
        }
        if removed > 0 {
            info!(
                target: "snapshot_init",
                "purged {} entries from {:?} before restore",
                removed, self.db_path,
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

        info!(
            target: "snapshot_init",
            "archive restore start archives={} archive_concurrency={}",
            manifest.archives.len(),
            self.archive_concurrency,
        );

        let archives_prefix = manifest.archives_prefix.clone();

        // worker pool:FuturesUnordered + 滑动窗口。
        // 任一 worker 错 → result? 短路返回 → in_flight 集合 drop → 余下 future 自动取消。
        let mut iter = manifest.archives.iter().cloned().enumerate();
        let mut in_flight = FuturesUnordered::new();
        for _ in 0..self.archive_concurrency {
            if let Some((idx, entry)) = iter.next() {
                in_flight.push(self.extract_one_archive(idx, entry, archives_prefix.clone()));
            }
        }
        while let Some(result) = in_flight.next().await {
            result?;
            if let Some((idx, entry)) = iter.next() {
                in_flight.push(self.extract_one_archive(idx, entry, archives_prefix.clone()));
            }
        }

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
    ) -> Result<()> {
        let key = format!("{}{}", archives_prefix, entry.name);
        info!(
            target: "snapshot_init",
            "archive start index={} name={} size_bytes={} files={}",
            idx, entry.name, entry.size_bytes, entry.file_count,
        );

        let mut last_err: Option<anyhow::Error> = None;
        let mut delay_ms = OUTER_RETRY_INITIAL_DELAY_MS;
        for attempt in 1..=self.outer_retry_attempts {
            match self.try_extract_one_archive(&key).await {
                Ok(()) => {
                    info!(
                        target: "snapshot_init",
                        "archive done index={} name={} attempt={}",
                        idx, entry.name, attempt,
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
    async fn try_extract_one_archive(&self, key: &str) -> Result<()> {
        let url = self.object_url(key);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("get {}", url))?
            .error_for_status()
            .with_context(|| format!("get {} returned error status", url))?;

        // bytes_stream → io::Error → StreamReader(AsyncRead)→ SyncIoBridge(sync Read)。
        // Box::pin 保证 StreamReader: Unpin(SyncIoBridge 的同步 Read impl 要求)。
        let stream = Box::pin(
            resp.bytes_stream()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)),
        );
        let async_read = StreamReader::new(stream);
        let db_path = self.db_path.clone();
        // SyncIoBridge 需要在 tokio runtime context 内构造(捕获 Handle),再 move 进 spawn_blocking。
        let sync_read = SyncIoBridge::new(async_read);
        tokio::task::spawn_blocking(move || extract_tar(sync_read, &db_path))
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
}

/// 同步流式解压:reader(假设未压缩 tar)→ tar entries → 直接落 db_path。
/// 在 spawn_blocking 内执行。单 archive 解压过程内存占用 KB 级(逐 entry 流式)。
fn extract_tar<R: std::io::Read>(reader: R, db_path: &Path) -> Result<()> {
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
