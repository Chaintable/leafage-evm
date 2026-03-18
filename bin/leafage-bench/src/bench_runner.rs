use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::corpus::types::{ClassLabel, CorpusCase};
use crate::corpus::Corpus;
use alloy::eips::BlockId;
use anyhow::Result;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{ContentArrangement, Table};
use jsonrpsee::core::client::Error;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::EthApiClient;
use leafage_evm_types::Bytes;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Debug)]
pub struct CaseResult {
    #[allow(dead_code)]
    pub case_id: String,
    pub label: ClassLabel,
    pub latency: Duration,
    pub outcome: Outcome,
}

pub type Outcome = std::result::Result<Bytes, jsonrpsee::core::client::error::Error>;

impl CaseResult {
    pub fn is_ok(&self) -> bool {
        match self.outcome {
            Ok(_) => true,
            Err(ref err) => match err {
                Error::Call(call_err) => {
                    let msg = call_err.message();
                    msg.contains("execution reverted") || msg.contains("Reverted")
                }
                _ => false,
            },
        }
    }
}

trait Metric {
    /// Sorted latency samples in nanoseconds
    fn latencies_ns<'a>(&'a self) -> &'a [u64];

    fn total_requests(&self) -> usize;

    fn total_errors(&self) -> usize;

    fn percentile_ms(&self, pct: f64) -> f64 {
        if self.latencies_ns().is_empty() {
            return 0.0;
        }
        let idx = Self::nearest_rank_index(self.latencies_ns().len(), pct);
        self.latencies_ns()[idx] as f64 / 1_000_000.0
    }
    fn error_rate(&self) -> f64 {
        if self.total_requests() == 0 {
            return 0.0;
        }
        self.total_errors() as f64 / self.total_requests() as f64 * 100.0
    }

    // Nearest-rank percentile index (1-based rank converted to 0-based index).
    fn nearest_rank_index(len: usize, pct: f64) -> usize {
        debug_assert!(len > 0);

        if !pct.is_finite() || pct <= 0.0 {
            return 0;
        }
        if pct >= 100.0 {
            return len - 1;
        }

        let rank = ((pct / 100.0) * len as f64).ceil() as usize;
        rank.saturating_sub(1).min(len - 1)
    }
}

/// Latency / error stats for a single complexity tier.
#[derive(Debug)]
pub struct LabelStats {
    pub total: usize,
    pub errors: usize,
    pub latencies_ns: Vec<u64>, // sorted
}

impl LabelStats {
    fn from_results(results: &[&CaseResult]) -> Self {
        let total = results.len();
        let errors = results.iter().filter(|r| !r.is_ok()).count();
        let mut latencies_ns: Vec<u64> = results
            .iter()
            .map(|r| r.latency.as_nanos() as u64)
            .collect();
        latencies_ns.sort_unstable();
        Self {
            total,
            errors,
            latencies_ns,
        }
    }
}

impl Metric for &LabelStats {
    fn latencies_ns(&self) -> &[u64] {
        &self.latencies_ns
    }

    fn total_requests(&self) -> usize {
        self.total
    }

    fn total_errors(&self) -> usize {
        self.errors
    }
}

#[derive(Debug)]
pub struct RunSummary {
    pub name: String,
    pub total: usize,
    pub errors: usize,
    pub duration: Duration,
    /// Sorted latency samples in nanoseconds for the overall run.
    pub latencies_ns: Vec<u64>,
    /// Per-label breakdown (L1 / L2 / L3).
    pub by_label: HashMap<ClassLabel, LabelStats>,
}

impl RunSummary {
    fn from_results(name: String, results: Vec<CaseResult>, duration: Duration) -> Self {
        let total = results.len();
        let errors = results.iter().filter(|r| !r.is_ok()).count();
        let mut latencies_ns: Vec<u64> = results
            .iter()
            .map(|r| r.latency.as_nanos() as u64)
            .collect();
        latencies_ns.sort_unstable();

        let mut by_label: HashMap<ClassLabel, Vec<&CaseResult>> = HashMap::new();
        for r in &results {
            by_label.entry(r.label).or_default().push(r);
        }
        let by_label = by_label
            .into_iter()
            .map(|(k, v)| (k, LabelStats::from_results(&v)))
            .collect();

        Self {
            name,
            total,
            errors,
            duration,
            latencies_ns,
            by_label,
        }
    }

    pub fn qps(&self) -> f64 {
        if self.duration.is_zero() {
            return 0.0;
        }
        self.total as f64 / self.duration.as_secs_f64()
    }
}

impl Metric for &RunSummary {
    fn latencies_ns(&self) -> &[u64] {
        &self.latencies_ns
    }

    fn total_requests(&self) -> usize {
        self.total
    }

    fn total_errors(&self) -> usize {
        self.errors
    }
}

impl std::fmt::Display for RunSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{} total={} errors={} ({:.1}%) duration={:.2}s qps={:.1}",
            self.name,
            self.total,
            self.errors,
            self.error_rate(),
            self.duration.as_secs_f64(),
            self.qps()
        )?;

        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header([
                "label", "count", "error%", "p50 ms", "p95 ms", "p99 ms", "p999 ms",
            ]);

        table.add_row([
            "overall".to_string(),
            self.total.to_string(),
            format!("{:.1}", self.error_rate()),
            format!("{:.2}", self.percentile_ms(50.0)),
            format!("{:.2}", self.percentile_ms(95.0)),
            format!("{:.2}", self.percentile_ms(99.0)),
            format!("{:.2}", self.percentile_ms(99.9)),
        ]);

        for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
            if let Some(s) = self.by_label.get(&label) {
                table.add_row([
                    label.as_str().to_string(),
                    s.total.to_string(),
                    format!("{:.1}", s.error_rate()),
                    format!("{:.2}", s.percentile_ms(50.0)),
                    format!("{:.2}", s.percentile_ms(95.0)),
                    format!("{:.2}", s.percentile_ms(99.0)),
                    format!("{:.2}", s.percentile_ms(99.9)),
                ]);
            }
        }

        write!(f, "{table}")
    }
}


pub struct BenchRunner {
    /// Primary endpoint client (leafage-evm).
    /// HttpClient is zero-copy clone internally, no Arc needed.
    target: HttpClient,
    /// Optional comparison endpoint client (geth).
    compare: Option<HttpClient>,
    concurrency: usize,
}

impl BenchRunner {
    pub fn new(target_url: &str, compare_url: Option<&str>, concurrency: usize) -> Result<Self> {
        let build = |url: &str| -> Result<HttpClient> {
            Ok(HttpClientBuilder::default()
                .request_timeout(Duration::from_secs(30))
                .build(url)?)
        };
        Ok(Self {
            target: build(target_url)?,
            compare: compare_url.map(build).transpose()?,
            concurrency,
        })
    }

    pub async fn run(&self, corpus: Corpus) -> Result<()> {
        if self.compare.is_some() {
            let (target_sum, compare_sum) = self.run_compare(&corpus.cases).await?;
            println!("{}", render_compare_report(&target_sum, &compare_sum));
        } else {
            let target_sum = self.run_target(&corpus.cases).await?;
            println!("{}", target_sum);
        }
        Ok(())
    }

    /// Run all cases against the primary endpoint and return aggregated results.
    pub async fn run_target(&self, cases: &[CorpusCase]) -> Result<RunSummary> {
        let (results, duration) =
            run_cases(self.target.clone(), cases.to_vec(), self.concurrency).await?;
        Ok(RunSummary::from_results("target".into(), results, duration))
    }

    /// Run all cases against both endpoints concurrently and return both summaries.
    ///
    /// The two endpoint runs are fired simultaneously; wall-clock time for each
    /// is measured independently.  Returns `(target, compare)`.
    pub async fn run_compare(&self, cases: &[CorpusCase]) -> Result<(RunSummary, RunSummary)> {
        let compare = self
            .compare
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("compare endpoint not configured"))?
            .clone();

        let target = self.target.clone();
        let concurrency = self.concurrency;
        let (target_res, compare_res) = tokio::try_join!(
            tokio::spawn(run_cases(target, cases.to_vec(), concurrency)),
            tokio::spawn(run_cases(compare, cases.to_vec(), concurrency)),
        )?;
        let target_res = target_res?;
        let compare_res = compare_res?;

        Ok((
            RunSummary::from_results("target".into(), target_res.0, target_res.1),
            RunSummary::from_results("compare".into(), compare_res.0, compare_res.1),
        ))
    }
}

/// Dispatch all cases to `client` with bounded concurrency.
///
/// All tasks are queued upfront and up to `concurrency` run in parallel at
/// any time (Semaphore), so there is no chunk-level serialisation.
/// Returns `(case_results, wall_clock_duration)`.
async fn run_cases(
    client: HttpClient,
    cases: Vec<CorpusCase>,
    concurrency: usize,
) -> Result<(Vec<CaseResult>, Duration)> {
    let sem = Arc::new(Semaphore::new(concurrency));
    let mut set = JoinSet::new();
    let wall_start = Instant::now();

    for case in cases.iter() {
        let client = client.clone();
        let sem = Arc::clone(&sem);
        let case_id = case.case_id.clone();
        let label = case.classification.label;
        let request = case.request.clone();
        let block_id = BlockId::latest();

        // All tasks are queued immediately.  Each task acquires the permit
        // itself, so the main loop is never blocked and true concurrency is
        // bounded by the semaphore without chunk-level serialisation.
        set.spawn(async move {
            let _permit = sem.acquire_owned().await?;
            let start = Instant::now();
            let outcome = client.call(request, block_id, None, None).await;
            Ok::<CaseResult, anyhow::Error>(CaseResult {
                case_id,
                label,
                latency: start.elapsed(),
                outcome,
            })
        });
    }

    let mut results = Vec::with_capacity(cases.len());
    while let Some(res) = set.join_next().await {
        results.push(res??);
    }

    Ok((results, wall_start.elapsed()))
}

fn render_compare_report(target: &RunSummary, compare: &RunSummary) -> String {
    let mut out = String::new();
    let _ = writeln!(&mut out, "{} vs {}", target.name, compare.name);

    let _ = writeln!(&mut out, "\n[overall]");
    let _ = writeln!(
        &mut out,
        "{}",
        render_table("overall", target, compare)
    );

    for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
        if !target.by_label.contains_key(&label) || !compare.by_label.contains_key(&label) {
            continue;
        }
        let target = target.by_label.get(&label).unwrap();
        let compare = compare.by_label.get(&label).unwrap();

        let _ = writeln!(&mut out, "\n[{}]", label.as_str());
        let _ = writeln!(
            &mut out,
            "{}",
            render_table(label.as_str(), target, compare)
        );
    }

    out
}

fn render_table(title: &str, t: impl Metric, c: impl Metric) -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(["metric", "target", "compare", "delta(compare-target)%"]);
    table.add_row([
        format!("{} count", title),
        t.total_requests().to_string(),
        c.total_requests().to_string(),
        format_delta_percent(t.total_requests() as f64, c.total_requests() as f64),
    ]);
    table.add_row([
        "error%".to_string(),
        format!("{:.2}", t.error_rate()),
        format!("{:.2}", c.error_rate()),
        format_delta_percent(t.error_rate(), c.error_rate()),
    ]);
    table.add_row([
        "p50 ms".to_string(),
        format!("{:.2}", t.percentile_ms(50.0)),
        format!("{:.2}", c.percentile_ms(50.0)),
        format_delta_percent(t.percentile_ms(50.0), c.percentile_ms(50.0)),
    ]);
    table.add_row([
        "p90 ms".to_string(),
        format!("{:.2}", t.percentile_ms(90.0)),
        format!("{:.2}", c.percentile_ms(90.0)),
        format_delta_percent(t.percentile_ms(90.0), c.percentile_ms(90.0)),
    ]);
    table.add_row([
        "p95 ms".to_string(),
        format!("{:.2}", t.percentile_ms(95.0)),
        format!("{:.2}", c.percentile_ms(95.0)),
        format_delta_percent(t.percentile_ms(95.0), c.percentile_ms(95.0)),
    ]);
    table.add_row([
        "p99 ms".to_string(),
        format!("{:.2}", t.percentile_ms(99.0)),
        format!("{:.2}", c.percentile_ms(99.0)),
        format_delta_percent(t.percentile_ms(99.0), c.percentile_ms(99.0)),
    ]);
    table.add_row([
        "p999 ms".to_string(),
        format!("{:.2}", t.percentile_ms(99.9)),
        format!("{:.2}", c.percentile_ms(99.9)),
        format_delta_percent(t.percentile_ms(99.9), c.percentile_ms(99.9)),
    ]);
    table
}

fn format_delta_percent(base: f64, new_value: f64) -> String {
    if base.abs() < f64::EPSILON {
        return "-".to_string();
    }
    let delta = (new_value - base) / base * 100.0;
    format!("{:+.2}%", delta)
}
