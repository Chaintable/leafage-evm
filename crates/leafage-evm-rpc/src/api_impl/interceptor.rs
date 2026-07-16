use crate::metrics::record_io_pressure_avg10;
use futures::TryFutureExt;
use hyper::{body::Bytes, Response, StatusCode};
use jsonrpsee::server::{HttpBody, HttpRequest};
use procfs::process::{Process, Stat};
use procfs::{Current, FromRead, IoPressure, WithCurrentSystemInfo};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicBool, AtomicU64},
    Arc,
};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::interval;
use tower::BoxError;
use tower::Layer;
use tower::Service;
use tracing::{debug, info, warn};

fn default_cpu_threshold() -> Vec<f64> {
    vec![65.0, 80.0, 95.0]
}

fn default_io_threshold() -> [f64; 3] {
    // PSI has no standardized low/middle/high levels. The 10% low watermark is based on:
    // systemd `some`: 200ms per 2s (~10%):
    // https://github.com/systemd/systemd/blob/main/src/basic/psi-util.h
    // PSI `some` semantics:
    // https://docs.kernel.org/accounting/psi.html#pressure-interface
    // The 20% and 50% watermarks are service-level load-shedding policy values.
    [10.0, 20.0, 50.0]
}

fn default_io_full_threshold() -> [f64; 3] {
    // The defaults use the Linux 5% trigger example and psi-notify's 15% default as anchors:
    // https://docs.kernel.org/accounting/psi.html#monitoring-for-pressure-thresholds
    // https://github.com/cdown/psi-notify#config
    [5.0, 10.0, 15.0]
}

fn default_max_retries() -> u64 {
    5
}

fn default_window() -> u64 {
    3
}

fn default_stat_interval() -> u64 {
    1000
}

fn default_cpu_window() -> u64 {
    10_000
}

fn default_not_retry_threshold() -> f64 {
    0.2
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InterceptorConfig {
    #[serde(default = "default_cpu_threshold")]
    pub cpu_threshold: Vec<f64>,
    /// IO PSI some.avg10 thresholds ordered as low, middle, and high.
    #[serde(default = "default_io_threshold")]
    pub io_threshold: [f64; 3],
    /// IO PSI full.avg10 thresholds ordered as low, middle, and high.
    #[serde(default = "default_io_full_threshold")]
    pub io_full_threshold: [f64; 3],
    #[serde(default = "default_max_retries")]
    pub max_retries: u64,
    #[serde(default = "default_window")]
    pub window: u64, // 窗口大小 (单位: 分钟)
    #[serde(default = "default_stat_interval")]
    pub stat_interval: u64, // 资源采样及状态刷新间隔 (单位: ms)
    #[serde(default = "default_cpu_window")]
    pub cpu_window: u64, // CPU 滑动平均窗口 (单位: ms)
    #[serde(default = "default_not_retry_threshold")]
    pub not_retry_threshold: f64, // 重试次数阈值
}

impl Default for InterceptorConfig {
    fn default() -> Self {
        InterceptorConfig {
            cpu_threshold: default_cpu_threshold(),
            io_threshold: default_io_threshold(),
            io_full_threshold: default_io_full_threshold(),
            max_retries: default_max_retries(),
            window: default_window(),
            stat_interval: default_stat_interval(),
            cpu_window: default_cpu_window(),
            not_retry_threshold: default_not_retry_threshold(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CpuSample {
    sampled_at: Instant,
    cpu_time: u64,
}

#[derive(Debug)]
struct CpuRecorder {
    samples: VecDeque<CpuSample>,
    window: Duration,
}

impl CpuRecorder {
    fn new(window: Duration, sampled_at: Instant, cpu_time: u64) -> Self {
        Self {
            samples: VecDeque::from([CpuSample {
                sampled_at,
                cpu_time,
            }]),
            window,
        }
    }

    fn record(
        &mut self,
        sampled_at: Instant,
        cpu_time: u64,
        ticks_per_second: u64,
        core_count: usize,
    ) -> Option<f64> {
        self.samples.push_back(CpuSample {
            sampled_at,
            cpu_time,
        });

        // Keep the newest sample that is at least one full window old. The next
        // sample, if present, is newer than the window boundary.
        while self.samples.len() >= 2
            && sampled_at.saturating_duration_since(self.samples[1].sampled_at) >= self.window
        {
            self.samples.pop_front();
        }

        let oldest = self.samples.front()?;
        let elapsed = sampled_at.saturating_duration_since(oldest.sampled_at);
        if elapsed < self.window {
            return None;
        }

        let cpu_time_diff = cpu_time.saturating_sub(oldest.cpu_time);
        Some(
            cpu_time_diff as f64
                / (ticks_per_second as f64 * core_count as f64 * elapsed.as_secs_f64())
                * 100.0,
        )
    }
}

#[derive(Debug, Clone)]
struct RetryEntry {
    minute: u64,                  // 分钟数
    request_counts: usize,        // 请求总数
    retry_weighted_counts: usize, // 重试请求总数
}

#[derive(Debug, Clone)]
struct RetryRecorder {
    entries: VecDeque<RetryEntry>,
    window: u64, // 窗口大小 (单位: 分钟)
}

impl RetryRecorder {
    fn new(window: u64) -> Self {
        RetryRecorder {
            entries: VecDeque::new(),
            window,
        }
    }

    fn add(&mut self, now_minute: u64, retries: u64) {
        if let Some(entry) = self.entries.back_mut() {
            if entry.minute == now_minute {
                entry.request_counts += 1;
                entry.retry_weighted_counts += retries as usize;
            } else {
                self.entries.push_back(RetryEntry {
                    minute: now_minute,
                    request_counts: 1,
                    retry_weighted_counts: retries as usize,
                });
            }
        } else {
            self.entries.push_back(RetryEntry {
                minute: now_minute,
                request_counts: 1,
                retry_weighted_counts: retries as usize,
            });
        }
    }

    // 清理旧数据
    fn clear(&mut self, now_minute: u64) {
        while let Some(entry) = self.entries.front() {
            if now_minute >= self.window + entry.minute {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }

    fn stat(&self) -> f64 {
        let mut total_request_counts = 0;
        let mut total_retry_weighted_counts = 0;
        for entry in &self.entries {
            total_request_counts += entry.request_counts;
            total_retry_weighted_counts += entry.retry_weighted_counts;
        }
        if total_request_counts == 0 {
            return 0.0;
        }
        total_retry_weighted_counts as f64 / total_request_counts as f64
    }
}

fn get_max_memory() -> Result<u64, Box<dyn std::error::Error>> {
    let cgroup_content = fs::read_to_string("/proc/self/cgroup")?;
    let cgroup_lines: Vec<&str> = cgroup_content.lines().collect();

    // 查找 memory 控制器路径
    let mut cgroup_path = PathBuf::from("/sys/fs/cgroup");
    for line in cgroup_lines {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 && parts[1].contains("memory") {
            cgroup_path.push(parts[2].trim_start_matches('/'));
            break;
        }
    }

    // 检查 cgroups v2
    let v2_max = cgroup_path.join("memory.max");
    if v2_max.exists() {
        let content = fs::read_to_string(&v2_max)?;
        let trimmed = content.trim();
        if trimmed == "max" {
            let meminfo = procfs::Meminfo::current()?;
            return Ok(meminfo.mem_total);
        } else {
            return Ok(trimmed.parse()?);
        }
    }

    // 检查 cgroups v1
    let v1_max = cgroup_path.join("memory.limit_in_bytes");
    if v1_max.exists() {
        let content = fs::read_to_string(&v1_max)?;
        let max_bytes: u64 = content.trim().parse()?;
        return Ok(max_bytes);
    }

    let meminfo = procfs::Meminfo::current()?;
    Ok(meminfo.mem_total)
}

fn discover_io_pressure_path(process: &Process) -> Option<PathBuf> {
    let cgroups = process.cgroups().ok()?;
    let cgroup = cgroups
        .into_iter()
        .find(|cgroup| cgroup.hierarchy == 0 && cgroup.controllers.is_empty())?;
    let mounts = process.mountinfo().ok()?;
    mounts
        .into_iter()
        .filter(|mount| mount.fs_type == "cgroup2")
        .map(|mount| cgroup_io_pressure_path(&mount.mount_point, &mount.root, &cgroup.pathname))
        .find(|path| path.exists())
}

fn cgroup_io_pressure_path(
    mount_point: &std::path::Path,
    mount_root: &str,
    cgroup_path: &str,
) -> PathBuf {
    let cgroup_path = std::path::Path::new(cgroup_path);
    let relative_path = cgroup_path
        .strip_prefix(mount_root)
        .or_else(|_| cgroup_path.strip_prefix("/"))
        .unwrap_or(cgroup_path);
    mount_point.join(relative_path).join("io.pressure")
}

pub struct InterceptorLayer {
    load_status: Arc<AtomicU64>,          // 记录当前的负载状态 百分比
    other_overloaded: Arc<AtomicBool>,    // 其他服务是否过载
    retries_sender: UnboundedSender<u64>, // 用于发送重试次数的通道
}

impl InterceptorLayer {
    pub fn new(cfg: &InterceptorConfig) -> Self {
        let (worker, retries_sender, load_status, other_overloaded) = InterceptorWorker::new(
            cfg.max_retries,
            cfg.window,
            cfg.cpu_threshold.clone(),
            cfg.io_threshold,
            cfg.io_full_threshold,
            cfg.stat_interval,
            cfg.cpu_window,
            cfg.not_retry_threshold,
        );
        let layer = InterceptorLayer {
            load_status,
            other_overloaded,
            retries_sender,
        };
        tokio::spawn(worker.run());
        layer
    }
}

impl<S> Layer<S> for InterceptorLayer {
    type Service = Interceptor<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Interceptor {
            inner,
            load_status: self.load_status.clone(),
            other_overloaded: self.other_overloaded.clone(),
            retries_sender: self.retries_sender.clone(),
        }
    }
}

struct InterceptorWorker {
    retry_recorder: RetryRecorder,
    load_status: Arc<AtomicU64>,
    other_overloaded: Arc<AtomicBool>,
    retries_receiver: UnboundedReceiver<u64>,
    process: Process,
    cpu_recorder: CpuRecorder,
    total_core_num: usize,
    total_mem_size: u64,
    io_pressure_path: Option<PathBuf>,
    stat_interval: u64,      // 采样间隔 (单位: ms)
    cpu_threshold: Vec<f64>, // CPU 使用率阈值, 例如 [45.0, 65.0, 85.0]
    io_threshold: [f64; 3],
    io_full_threshold: [f64; 3],
    not_retry_threshold: f64,
}

impl InterceptorWorker {
    fn new(
        max_retries: u64,
        window: u64,
        cpu_threshold: Vec<f64>,
        io_threshold: [f64; 3],
        io_full_threshold: [f64; 3],
        stat_interval: u64,
        cpu_window: u64,
        not_retry_threshold: f64,
    ) -> (Self, UnboundedSender<u64>, Arc<AtomicU64>, Arc<AtomicBool>) {
        if cpu_threshold.len() != 3 {
            panic!("cpu_threshold must have exactly 3 values");
        }
        for threshold in &cpu_threshold {
            if *threshold < 0.0 || *threshold > 100.0 {
                panic!("cpu_threshold values must be between 0.0 and 100.0");
            }
        }
        validate_io_threshold(io_threshold);
        validate_io_threshold(io_full_threshold);
        validate_sampling_intervals(stat_interval, cpu_window);
        if cpu_window < stat_interval {
            warn!(
                target = "interceptor",
                stat_interval,
                cpu_window,
                "cpu_window is shorter than stat_interval; the effective CPU window will be limited by the sampling interval"
            );
        }
        let (retries_sender, retries_receiver) = unbounded_channel();
        let load_status = Arc::new(AtomicU64::new(0));
        let other_overloaded = Arc::new(AtomicBool::new(false));
        let total_core_num = std::thread::available_parallelism().unwrap().get();
        let process = Process::myself().unwrap();
        let initial_stat = process.stat().unwrap();
        let cpu_recorder = CpuRecorder::new(
            Duration::from_millis(cpu_window),
            Instant::now(),
            total_cpu_time(&initial_stat),
        );
        let total_mem_size = get_max_memory().unwrap_or(0);
        let io_pressure_path = discover_io_pressure_path(&process);
        let worker = InterceptorWorker {
            retry_recorder: RetryRecorder::new(window),
            load_status: load_status.clone(),
            other_overloaded: other_overloaded.clone(),
            retries_receiver,
            process,
            cpu_recorder,
            total_core_num,
            total_mem_size,
            io_pressure_path: io_pressure_path.clone(),
            stat_interval,
            cpu_threshold,
            io_threshold,
            io_full_threshold,
            not_retry_threshold,
        };
        info!(
            target = "interceptor",
            "InterceptorWorker initialized with max_retries: {}, window: {:?}, stat_interval: {}ms, cpu_window: {}ms, total_core_num: {}, total_mem_size: {}, io_pressure_path: {:?}",
            max_retries,
            window,
            stat_interval,
            cpu_window,
            total_core_num,
            total_mem_size,
            io_pressure_path
        );
        if worker.io_pressure_path.is_none() {
            warn!(
                target = "interceptor",
                "cgroup v2 IO pressure information is unavailable; IO overload protection will fail open"
            );
        }
        (worker, retries_sender, load_status, other_overloaded)
    }

    fn cpu_load_status(&self, cpu_usage: f64) -> LoadStatus {
        if cpu_usage <= self.cpu_threshold[0] {
            LoadStatus::NoRefused
        } else if cpu_usage <= self.cpu_threshold[1] {
            LoadStatus::LowRefused
        } else if cpu_usage <= self.cpu_threshold[2] {
            LoadStatus::MiddleRefused
        } else {
            LoadStatus::AllRefused
        }
    }

    fn io_load_status(&self) -> LoadStatus {
        let Some(path) = self.io_pressure_path.as_ref() else {
            return LoadStatus::NoRefused;
        };

        match IoPressure::from_file(path) {
            Ok(pressure) => {
                record_io_pressure_avg10(pressure.some.avg10, pressure.full.avg10);
                io_pressure_load_status(
                    pressure.some.avg10,
                    pressure.full.avg10,
                    self.io_threshold,
                    self.io_full_threshold,
                )
            }
            Err(err) => {
                debug!(
                    target = "interceptor",
                    path = %path.display(),
                    error = %err,
                    "Failed to sample IO pressure; IO overload protection will fail open"
                );
                LoadStatus::NoRefused
            }
        }
    }

    fn set_load_status(&mut self) {
        let stat = self.process.stat().unwrap();
        let cpu_usage = self.cpu_recorder.record(
            Instant::now(),
            total_cpu_time(&stat),
            procfs::ticks_per_second(),
            self.total_core_num,
        );
        let mem_usage = stat.rss_bytes().get() as f64 / self.total_mem_size as f64 * 100.0;
        let cpu_status = cpu_usage
            .map(|usage| self.cpu_load_status(usage))
            .unwrap_or(LoadStatus::NoRefused);
        let io_status = self.io_load_status();
        let status = cpu_status.max(io_status);
        self.load_status
            .store(status as u64, std::sync::atomic::Ordering::SeqCst);
        debug!(
            target = "interceptor",
            "Load status updated: {status:?} (cpu_status: {cpu_status:?}, io_status: {io_status:?}, usage: {cpu_usage:.2?}%, total_core_num: {}, mem_usage: {mem_usage:.2}%, total_mem_size: {})",
            self.total_core_num, self.total_mem_size);
    }

    fn set_other_overloaded(&mut self) {
        let stat = self.retry_recorder.stat();
        self.other_overloaded.store(
            stat >= self.not_retry_threshold,
            std::sync::atomic::Ordering::SeqCst,
        );
        debug!(
            target = "interceptor",
            "Other overloaded status updated: {overloaded}, retry rate: {stat:.2}",
            overloaded = self
                .other_overloaded
                .load(std::sync::atomic::Ordering::SeqCst),
            stat = stat
        );
    }

    async fn run(mut self) {
        let mut interval = interval(Duration::from_millis(self.stat_interval));
        let mut now_minute = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            / 60;
        loop {
            tokio::select! {
                msg = self.retries_receiver.recv() => {
                    if let Some(retries) = msg {
                        self.retry_recorder.add(now_minute, retries);
                    } else {
                        break;
                    }
                }
                _ = interval.tick() => {
                    self.set_load_status();
                    self.set_other_overloaded();
                    now_minute = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        / 60;
                    self.retry_recorder.clear(now_minute);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u64)]
enum LoadStatus {
    NoRefused = 0,
    LowRefused = 1,
    MiddleRefused = 2,
    AllRefused = 3,
}

fn validate_io_threshold(thresholds: [f64; 3]) {
    if thresholds
        .iter()
        .any(|threshold| !(0.0..=100.0).contains(threshold))
    {
        panic!("io_threshold values must be between 0.0 and 100.0");
    }
    if thresholds.windows(2).any(|pair| pair[0] >= pair[1]) {
        panic!("io_threshold values must be strictly increasing");
    }
}

fn validate_sampling_intervals(stat_interval: u64, cpu_window: u64) {
    assert!(stat_interval > 0, "stat_interval must be greater than 0");
    assert!(cpu_window > 0, "cpu_window must be greater than 0");
}

fn total_cpu_time(stat: &Stat) -> u64 {
    stat.utime.saturating_add(stat.stime)
}

fn io_pressure_load_status(
    some_avg10: f32,
    full_avg10: f32,
    some_thresholds: [f64; 3],
    full_thresholds: [f64; 3],
) -> LoadStatus {
    let some_avg10 = some_avg10 as f64;
    let full_avg10 = full_avg10 as f64;

    if some_avg10 >= some_thresholds[2] || full_avg10 >= full_thresholds[2] {
        LoadStatus::AllRefused
    } else if some_avg10 >= some_thresholds[1] || full_avg10 >= full_thresholds[1] {
        LoadStatus::MiddleRefused
    } else if some_avg10 >= some_thresholds[0] || full_avg10 >= full_thresholds[0] {
        LoadStatus::LowRefused
    } else {
        LoadStatus::NoRefused
    }
}

impl From<u64> for LoadStatus {
    fn from(value: u64) -> Self {
        match value {
            0 => LoadStatus::NoRefused,
            1 => LoadStatus::LowRefused,
            2 => LoadStatus::MiddleRefused,
            3 => LoadStatus::AllRefused,
            _ => LoadStatus::AllRefused,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Interceptor<T> {
    inner: T,
    load_status: Arc<AtomicU64>,          // 记录当前的负载状态 百分比
    other_overloaded: Arc<AtomicBool>,    // 其他服务是否过载
    retries_sender: UnboundedSender<u64>, // 用于发送重试次数的通道
}

impl<T> Interceptor<T> {
    // 规则1: 检查x-load-deadline
    fn check_load_deadline(&self, load_shedding: &LoadShedding) -> bool {
        if let Some(deadline) = load_shedding.load_deadline {
            let now = SystemTime::now();
            if deadline < now {
                return true;
            }
        }
        false
    }

    // 规则2: 执行负载评估
    fn check_load_status(&self, load_shedding: &LoadShedding) -> bool {
        let load_status =
            LoadStatus::from(self.load_status.load(std::sync::atomic::Ordering::SeqCst));
        match load_status {
            LoadStatus::NoRefused => false,
            LoadStatus::LowRefused => return load_shedding.load_priority == LoadPriority::Low,
            LoadStatus::MiddleRefused => {
                if load_shedding.load_priority == LoadPriority::Low
                    || load_shedding.load_priority == LoadPriority::Medium
                {
                    return true;
                }
                false
            }
            LoadStatus::AllRefused => true,
        }
    }

    // 规则3: 检查其他服务是否过载
    fn check_other_overloaded(&self) -> bool {
        self.other_overloaded
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    // 记录请求
    fn record_request(&self, load_shedding: &LoadShedding) {
        let _ = self.retries_sender.send(load_shedding.load_retries);
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum LoadSheddingParseError {
    #[error("Invalid load priority: {0}")]
    LoadPriorityParseError(String),
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum LoadPriority {
    High,
    Medium,
    Low,
}

impl FromStr for LoadPriority {
    type Err = LoadSheddingParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "high" | "HIGH" => Ok(LoadPriority::High),
            "medium" | "MEDIUM" => Ok(LoadPriority::Medium),
            "low" | "LOW" => Ok(LoadPriority::Low),
            _ => Err(LoadSheddingParseError::LoadPriorityParseError(
                s.to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone)]
struct LoadShedding {
    load_priority: LoadPriority,
    load_deadline: Option<SystemTime>,
    load_retries: u64,
}

impl From<&http::header::HeaderMap> for LoadShedding {
    fn from(headers: &http::header::HeaderMap) -> Self {
        let load_priority = headers
            .get("x-load-priority")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| LoadPriority::from_str(s).ok())
            .unwrap_or(LoadPriority::High);

        let load_deadline = headers
            .get("x-load-deadline")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| u64::from_str(s).ok())
            .map(|v| SystemTime::UNIX_EPOCH + Duration::from_secs(v));

        let load_retries = headers
            .get("x-load-retries")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| u64::from_str(s).ok())
            .unwrap_or(0);

        LoadShedding {
            load_priority,
            load_deadline,
            load_retries,
        }
    }
}

impl<S, B> Service<HttpRequest<B>> for Interceptor<S>
where
    S: Service<HttpRequest<B>, Response = Response<HttpBody>>,
    S::Response: 'static,
    S::Error: Into<BoxError> + 'static,
    S::Future: Send + 'static,
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    type Response = S::Response;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, request: HttpRequest<B>) -> Self::Future {
        // 检查是否跳过拦截
        if request.headers().contains_key("x-skip-interceptor") {
            return Box::pin(self.inner.call(request).map_err(Into::into));
        }

        let load_shedding = LoadShedding::from(request.headers());
        // 请求超过终止时间
        if self.check_load_deadline(&load_shedding) {
            let response = http::response::Builder::new()
                .status(StatusCode::REQUEST_TIMEOUT)
                .body(HttpBody::from("Request expired"))
                .unwrap();
            return Box::pin(async move { Ok(response) });
        }
        // 记录请求
        self.record_request(&load_shedding);

        // 根据负载状态决定是否拒绝请求
        let reject = self.check_load_status(&load_shedding);

        if reject {
            // 判断其它节点的负载情况
            let other_overloaded = self.check_other_overloaded();
            if other_overloaded {
                let mut response = http::response::Builder::new()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body(HttpBody::from("Service overloaded, no retry needed"))
                    .unwrap();
                response
                    .headers_mut()
                    .insert("x-load-not-retry", "true".parse().unwrap());
                return Box::pin(async move { Ok(response) });
            } else {
                let response = http::response::Builder::new()
                    .status(StatusCode::TOO_MANY_REQUESTS)
                    .body(HttpBody::from("Service overloaded"))
                    .unwrap();
                return Box::pin(async move { Ok(response) });
            }
        }
        Box::pin(self.inner.call(request).map_err(Into::into))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::future::{ready, Ready};
    use std::io::Cursor;

    struct OkService;

    impl Service<HttpRequest<HttpBody>> for OkService {
        type Response = Response<HttpBody>;
        type Error = Infallible;
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _request: HttpRequest<HttpBody>) -> Self::Future {
            ready(Ok(Response::new(HttpBody::from("ok"))))
        }
    }

    #[test]
    fn test_max_memory() {
        let a = get_max_memory();
        assert!(a.is_ok(), "Failed to get max memory: {:?}", a.err());
        let max_memory = a.unwrap();
        assert!(max_memory > 0, "Max memory should be greater than 0");
        dbg!("Max memory: {max_memory}");
    }

    #[test]
    fn test_load_priority() {
        assert_eq!(LoadPriority::from_str("high").unwrap(), LoadPriority::High);
        assert_eq!(
            LoadPriority::from_str("medium").unwrap(),
            LoadPriority::Medium
        );
        assert_eq!(LoadPriority::from_str("low").unwrap(), LoadPriority::Low);
        assert!(LoadPriority::from_str("invalid").is_err());
    }

    #[test]
    fn test_io_threshold_config() {
        let default_config: InterceptorConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(default_config.io_threshold, default_io_threshold());
        assert_eq!(
            default_config.io_full_threshold,
            default_io_full_threshold()
        );

        let custom_config: InterceptorConfig = serde_json::from_str(
            r#"{"io_threshold":[5.0,15.0,25.0],"io_full_threshold":[1.0,3.0,8.0]}"#,
        )
        .unwrap();
        assert_eq!(custom_config.io_threshold, [5.0, 15.0, 25.0]);
        assert_eq!(custom_config.io_full_threshold, [1.0, 3.0, 8.0]);
    }

    #[test]
    fn test_cpu_window_config() {
        let default_config: InterceptorConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(default_config.stat_interval, 1_000);
        assert_eq!(default_config.cpu_window, 10_000);

        let custom_config: InterceptorConfig =
            serde_json::from_str(r#"{"stat_interval":500,"cpu_window":5000}"#).unwrap();
        assert_eq!(custom_config.stat_interval, 500);
        assert_eq!(custom_config.cpu_window, 5_000);
    }

    #[test]
    fn test_cpu_recorder_waits_for_full_window() {
        let start = Instant::now();
        let mut recorder = CpuRecorder::new(Duration::from_secs(10), start, 0);

        for second in 1..10 {
            assert_eq!(
                recorder.record(start + Duration::from_secs(second), second * 100, 100, 1),
                None
            );
        }
    }

    #[test]
    fn test_cpu_recorder_calculates_rolling_average() {
        let start = Instant::now();
        let mut recorder = CpuRecorder::new(Duration::from_secs(10), start, 0);

        for second in 1..=9 {
            recorder.record(start + Duration::from_secs(second), second * 50, 100, 1);
        }

        // Nine seconds at 50% plus one second at 100% produces a 55% average.
        let usage = recorder
            .record(start + Duration::from_secs(10), 550, 100, 1)
            .unwrap();
        assert!((usage - 55.0).abs() < f64::EPSILON);

        // The next update drops the oldest second and still uses a ten-second window.
        let usage = recorder
            .record(start + Duration::from_secs(11), 650, 100, 1)
            .unwrap();
        assert!((usage - 60.0).abs() < f64::EPSILON);
        assert_eq!(recorder.samples.len(), 11);
    }

    #[test]
    fn test_cpu_recorder_uses_actual_elapsed_for_irregular_samples() {
        let start = Instant::now();
        let mut recorder = CpuRecorder::new(Duration::from_secs(10), start, 0);

        assert_eq!(
            recorder.record(start + Duration::from_secs(4), 200, 100, 1),
            None
        );
        assert_eq!(
            recorder.record(start + Duration::from_secs(9), 700, 100, 1),
            None
        );

        // At 15s the boundary is 5s. The 4s sample is the closest sample before
        // the boundary, so the effective elapsed time is 11s, not a fixed 10s.
        let usage = recorder
            .record(start + Duration::from_secs(15), 1_000, 100, 1)
            .unwrap();
        assert!((usage - (800.0 / 11.0)).abs() < 1e-10);
        assert_eq!(
            recorder.samples.front().unwrap().sampled_at,
            start + Duration::from_secs(4)
        );
    }

    #[test]
    fn test_sampling_interval_may_exceed_cpu_window_for_backward_compatibility() {
        validate_sampling_intervals(20_000, 10_000);
    }

    #[test]
    #[should_panic(expected = "cpu_window must be greater than 0")]
    fn test_cpu_window_must_be_nonzero() {
        validate_sampling_intervals(1_000, 0);
    }

    #[test]
    #[should_panic(expected = "io_threshold values must be strictly increasing")]
    fn test_io_threshold_must_be_increasing() {
        let mut thresholds = default_io_threshold();
        thresholds[1] = thresholds[0];
        validate_io_threshold(thresholds);
    }

    #[test]
    fn test_io_pressure_load_status() {
        let thresholds = default_io_threshold();
        let full_thresholds = [1.0, 3.0, 8.0];
        let status = |some, full| io_pressure_load_status(some, full, thresholds, full_thresholds);

        assert_eq!(status(9.99, 0.99), LoadStatus::NoRefused);
        assert_eq!(status(10.0, 0.0), LoadStatus::LowRefused);
        assert_eq!(status(0.0, 1.0), LoadStatus::LowRefused);
        assert_eq!(status(20.0, 0.0), LoadStatus::MiddleRefused);
        assert_eq!(status(0.0, 3.0), LoadStatus::MiddleRefused);
        assert_eq!(status(49.99, 0.0), LoadStatus::MiddleRefused);
        assert_eq!(status(50.0, 0.0), LoadStatus::AllRefused);
        assert_eq!(status(0.0, 8.0), LoadStatus::AllRefused);
    }

    #[test]
    fn test_cgroup_io_pressure_path() {
        let mount_point = std::path::Path::new("/sys/fs/cgroup");
        assert_eq!(
            cgroup_io_pressure_path(mount_point, "/", "/"),
            PathBuf::from("/sys/fs/cgroup/io.pressure")
        );
        assert_eq!(
            cgroup_io_pressure_path(mount_point, "/", "/docker/container-id"),
            PathBuf::from("/sys/fs/cgroup/docker/container-id/io.pressure")
        );
        assert_eq!(
            cgroup_io_pressure_path(mount_point, "/docker/container-id", "/"),
            PathBuf::from("/sys/fs/cgroup/io.pressure")
        );
        assert_eq!(
            cgroup_io_pressure_path(
                mount_point,
                "/docker/container-id",
                "/docker/container-id/workload"
            ),
            PathBuf::from("/sys/fs/cgroup/workload/io.pressure")
        );
    }

    #[test]
    fn test_parse_io_pressure() {
        let input = b"some avg10=12.34 avg60=5.67 avg300=1.23 total=123456\n\
                      full avg10=2.50 avg60=1.00 avg300=0.25 total=65432\n";
        let pressure = IoPressure::from_read(Cursor::new(input)).unwrap();

        assert_eq!(pressure.some.avg10, 12.34);
        assert_eq!(pressure.full.avg10, 2.5);
        assert_eq!(
            io_pressure_load_status(
                pressure.some.avg10,
                pressure.full.avg10,
                default_io_threshold(),
                default_io_full_threshold(),
            ),
            LoadStatus::LowRefused
        );
    }

    #[test]
    fn test_load_status_uses_highest_pressure() {
        assert_eq!(
            LoadStatus::LowRefused.max(LoadStatus::MiddleRefused),
            LoadStatus::MiddleRefused
        );
        assert_eq!(
            LoadStatus::AllRefused.max(LoadStatus::NoRefused),
            LoadStatus::AllRefused
        );
    }

    #[test]
    fn test_request_rejection_by_load_status() {
        let (retries_sender, _retries_receiver) = unbounded_channel();
        let interceptor = Interceptor {
            inner: (),
            load_status: Arc::new(AtomicU64::new(LoadStatus::NoRefused as u64)),
            other_overloaded: Arc::new(AtomicBool::new(false)),
            retries_sender,
        };

        let request = |load_priority| LoadShedding {
            load_priority,
            load_deadline: None,
            load_retries: 0,
        };

        let set_status = |status| {
            interceptor
                .load_status
                .store(status as u64, std::sync::atomic::Ordering::SeqCst);
        };

        assert!(!interceptor.check_load_status(&request(LoadPriority::High)));
        assert!(!interceptor.check_load_status(&request(LoadPriority::Medium)));
        assert!(!interceptor.check_load_status(&request(LoadPriority::Low)));

        set_status(LoadStatus::LowRefused);
        assert!(!interceptor.check_load_status(&request(LoadPriority::High)));
        assert!(!interceptor.check_load_status(&request(LoadPriority::Medium)));
        assert!(interceptor.check_load_status(&request(LoadPriority::Low)));

        set_status(LoadStatus::MiddleRefused);
        assert!(!interceptor.check_load_status(&request(LoadPriority::High)));
        assert!(interceptor.check_load_status(&request(LoadPriority::Medium)));
        assert!(interceptor.check_load_status(&request(LoadPriority::Low)));

        set_status(LoadStatus::AllRefused);
        assert!(interceptor.check_load_status(&request(LoadPriority::High)));
        assert!(interceptor.check_load_status(&request(LoadPriority::Medium)));
        assert!(interceptor.check_load_status(&request(LoadPriority::Low)));
    }

    #[tokio::test]
    async fn test_rejected_request_returns_429() {
        let (retries_sender, _retries_receiver) = unbounded_channel();
        let mut interceptor = Interceptor {
            inner: OkService,
            load_status: Arc::new(AtomicU64::new(LoadStatus::LowRefused as u64)),
            other_overloaded: Arc::new(AtomicBool::new(false)),
            retries_sender,
        };
        let request = HttpRequest::builder()
            .header("x-load-priority", "low")
            .body(HttpBody::from(""))
            .unwrap();

        let response = interceptor.call(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn test_retry_recorder() {
        let mut recorder = RetryRecorder::new(3);
        recorder.add(1, 1);
        recorder.add(1, 2);
        recorder.clear(1);
        assert_eq!(recorder.stat(), 1.5);

        recorder.add(2, 0);
        assert_eq!(recorder.stat(), 1.0);
        recorder.add(3, 3);
        assert_eq!(recorder.stat(), 1.5);
        recorder.add(4, 0);
        assert_eq!(recorder.stat(), 1.2);
        recorder.clear(4);
        dbg!(&recorder.entries);
        assert_eq!(recorder.stat(), 1.0);
    }
}
