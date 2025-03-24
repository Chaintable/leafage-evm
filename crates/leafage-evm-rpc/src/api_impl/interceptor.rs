use futures::TryFutureExt;
use hyper::{body::Bytes, Response, StatusCode};
use jsonrpsee::server::{HttpBody, HttpRequest};
use procfs::process::{Process, Stat};
use procfs::{Current, WithCurrentSystemInfo};
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
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::interval;
use tower::BoxError;
use tower::Layer;
use tower::Service;
use tracing::{debug, info};

fn default_cpu_threshold() -> Vec<f64> {
    vec![45.0, 65.0, 85.0]
}

fn default_max_retries() -> u64 {
    5
}

fn default_window() -> u64 {
    180
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InterceptorConfig {
    #[serde(default = "default_cpu_threshold")]
    pub cpu_threshold: Vec<f64>,
    #[serde(default = "default_max_retries")]
    pub max_retries: u64,
    #[serde(default = "default_window")]
    pub window: u64,
}

impl Default for InterceptorConfig {
    fn default() -> Self {
        InterceptorConfig {
            cpu_threshold: default_cpu_threshold(),
            max_retries: default_max_retries(),
            window: default_window(),
        }
    }
}

struct RetryEntry {
    timestamp: Instant,
    retries: u64, // 重试次数
}

// 重试次数直方图
struct RetryHistogram {
    entries: VecDeque<RetryEntry>,
    buckets: Vec<u64>, // 直方图桶，记录每个重试次数的出现频率
    max_retries: u64,  // 最大跟踪的重试次数
    window: Duration,
}

impl RetryHistogram {
    fn new(max_retries: u64, window: Duration) -> Self {
        RetryHistogram {
            entries: VecDeque::new(),
            buckets: vec![0; (max_retries + 1) as usize], // +1 因为包含0次重试
            max_retries,
            window,
        }
    }

    fn add(&mut self, retries: u64) {
        let now = Instant::now();
        let retries = retries.min(self.max_retries); // 限制最大重试次数
        self.entries.push_back(RetryEntry {
            timestamp: now,
            retries,
        });

        self.buckets[retries as usize] += 1;

        self.clear(now);
    }

    // 清理旧数据
    fn clear(&mut self, now: Instant) {
        while let Some(entry) = self.entries.front() {
            if now.duration_since(entry.timestamp) > self.window {
                self.buckets[entry.retries as usize] -= 1;
                self.entries.pop_front();
            } else {
                break;
            }
        }
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

pub struct InterceptorLayer {
    load_status: Arc<AtomicU64>,          // 记录当前的负载状态 百分比
    other_overloaded: Arc<AtomicBool>,    // 其他服务是否过载
    retries_sender: UnboundedSender<u64>, // 用于发送重试次数的通道
}

impl InterceptorLayer {
    pub fn new(cfg: &InterceptorConfig) -> Self {
        let (worker, retries_sender, load_status, other_overloaded) = InterceptorWorker::new(
            cfg.max_retries,
            Duration::from_secs(cfg.window),
            cfg.cpu_threshold.clone(),
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
    retry_histogram: RetryHistogram,
    load_status: Arc<AtomicU64>,
    other_overloaded: Arc<AtomicBool>,
    retries_receiver: UnboundedReceiver<u64>,
    process: Process,
    latest_stat: Stat,
    total_core_num: usize,
    total_mem_size: u64,
    stat_interval: u64,      // 采样间隔 (单位: ms)
    cpu_threshold: Vec<f64>, // CPU 使用率阈值, 例如 [45.0, 65.0, 85.0]
}

impl InterceptorWorker {
    fn new(
        max_retries: u64,
        window: Duration,
        cpu_threshold: Vec<f64>,
    ) -> (Self, UnboundedSender<u64>, Arc<AtomicU64>, Arc<AtomicBool>) {
        if cpu_threshold.len() != 3 {
            panic!("cpu_threshold must have exactly 3 values");
        }
        for threshold in &cpu_threshold {
            if *threshold < 0.0 || *threshold > 100.0 {
                panic!("cpu_threshold values must be between 0.0 and 100.0");
            }
        }
        let (retries_sender, retries_receiver) = unbounded_channel();
        let load_status = Arc::new(AtomicU64::new(0));
        let other_overloaded = Arc::new(AtomicBool::new(false));
        let total_core_num = std::thread::available_parallelism().unwrap().get();
        let process = Process::myself().unwrap();
        let latest_stat = process.stat().unwrap();
        let total_mem_size = get_max_memory().unwrap_or(0);
        let worker = InterceptorWorker {
            retry_histogram: RetryHistogram::new(max_retries, window),
            load_status: load_status.clone(),
            other_overloaded: other_overloaded.clone(),
            retries_receiver,
            process,
            latest_stat,
            total_core_num,
            total_mem_size,
            stat_interval: 1000, // 采样间隔设置为1秒
            cpu_threshold,
        };
        debug!(
            target = "interceptor",
            "InterceptorWorker initialized with max_retries: {}, window: {:?}, total_core_num: {}",
            max_retries,
            window,
            total_core_num
        );
        (worker, retries_sender, load_status, other_overloaded)
    }

    fn check_load_status(&self, cpu_usage: f64, _mem_usage: f64) -> u64 {
        if cpu_usage <= self.cpu_threshold[0] {
            0 // NoRefused
        } else if cpu_usage <= self.cpu_threshold[1] {
            1 // LowRefused
        } else if cpu_usage <= self.cpu_threshold[2] {
            2 // MiddleRefused
        } else {
            3 // AllRefused
        }
    }

    fn set_load_status(&mut self) {
        let stat = self.process.stat().unwrap();
        let latest_stat = self.latest_stat.clone();
        let cpu_time_diff = stat.utime + stat.stime - latest_stat.utime - latest_stat.stime;
        let clk_tck = procfs::ticks_per_second();
        let cpu_usage = (cpu_time_diff as f64
            / (clk_tck as f64 * self.total_core_num as f64 * self.stat_interval as f64 / 1000.0))
            * 100.0;
        let mem_usage = stat.rss_bytes().get() as f64 / self.total_mem_size as f64 * 100.0;
        let status = self.check_load_status(cpu_usage, mem_usage);
        self.load_status
            .store(status, std::sync::atomic::Ordering::SeqCst);
        info!(
            target = "interceptor",
            "Load status updated: {status} (usage: {cpu_usage:.2}%, total_core_num: {}, mem_usage: {mem_usage:.2}%, total_mem_size: {})",
            self.total_core_num, self.total_mem_size);
        self.latest_stat = stat;
    }

    fn set_other_overloaded(&self) {
        self.other_overloaded
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    async fn run(mut self) {
        let mut interval = interval(Duration::from_millis(self.stat_interval));
        loop {
            tokio::select! {
                msg = self.retries_receiver.recv() => {
                    if let Some(retries) = msg {
                        self.retry_histogram.add(retries);
                    } else {
                        break; // 处理关闭通道的情况
                    }
                }
                _ = interval.tick() => {
                    self.set_load_status();
                    self.set_other_overloaded();
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u64)]
enum LoadStatus {
    NoRefused = 0,
    LowRefused = 1,
    MiddleRefused = 2,
    AllRefused = 3,
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
    fn check(&self, load_shedding: &LoadShedding) -> (bool, bool) {
        let (mut reject, mut retryable) = self.check_load_deadline(load_shedding);
        if reject {
            return (reject, retryable);
        }
        reject = self.check_load_status(load_shedding);
        if reject {
            retryable = !self.check_other_overloaded();
        }
        self.record_request(load_shedding, reject);
        return (reject, retryable);
    }

    // 规则1: 检查x-load-deadline
    fn check_load_deadline(&self, load_shedding: &LoadShedding) -> (bool, bool) {
        let now = SystemTime::now();
        if load_shedding.load_deadline < now {
            return (true, false);
        }
        (false, false)
    }

    // 规则2: 执行负载评估
    fn check_load_status(&self, load_shedding: &LoadShedding) -> bool {
        let load_status =
            LoadStatus::from(self.load_status.load(std::sync::atomic::Ordering::SeqCst));
        match load_status {
            LoadStatus::NoRefused => false,
            LoadStatus::LowRefused => return load_shedding.load_priority == LoadPriority::Low,
            LoadStatus::MiddleRefused => {
                if load_shedding.load_priority == LoadPriority::High
                    || load_shedding.load_priority == LoadPriority::Medium
                {
                    return true;
                }
                false
            }
            LoadStatus::AllRefused => true,
        }
    }

    fn check_other_overloaded(&self) -> bool {
        self.other_overloaded
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn record_request(&self, load_shedding: &LoadShedding, _reject: bool) {
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
    load_deadline: SystemTime,
    load_retries: u64,
}

impl LoadShedding {
    fn get(headers: &http::header::HeaderMap) -> Option<Self> {
        let load_priority = headers
            .get("x-load-priority")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| LoadPriority::from_str(s).ok())?;
        let load_deadline = headers
            .get("x-load-deadline")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| u64::from_str(s).ok())
            .map(|v| SystemTime::now() + Duration::from_secs(v))?;
        let load_retries = headers
            .get("x-load-retries")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| u64::from_str(s).ok())?;
        Some(LoadShedding {
            load_priority,
            load_deadline,
            load_retries,
        })
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
        let load_shedding = LoadShedding::get(request.headers());
        let (reject, retryable) = if let Some(load_shedding) = load_shedding {
            self.check(&load_shedding)
        } else {
            (false, false)
        };
        if reject {
            let mut response = http::response::Builder::new()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .body(HttpBody::empty())
                .unwrap();
            if !retryable {
                response
                    .headers_mut()
                    .insert("x-load-not-retry", "true".parse().unwrap());
            }
            return Box::pin(async move { Ok(response) });
        }
        Box::pin(self.inner.call(request).map_err(Into::into))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_max_memory() {
        let a = get_max_memory();
        assert!(a.is_ok(), "Failed to get max memory: {:?}", a.err());
        let max_memory = a.unwrap();
        assert!(max_memory > 0, "Max memory should be greater than 0");
        dbg!("Max memory: {max_memory}");
    }
}
