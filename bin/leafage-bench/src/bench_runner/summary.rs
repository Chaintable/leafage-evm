use crate::bench_runner::render::table::RenderWrapper;
use crate::bench_runner::render::{fmt_mean_std, Render};
use crate::bench_runner::CaseResult;
use crate::corpus::ClassLabel;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{ContentArrangement, Table};
use std::collections::HashMap;
use std::time::Duration;

pub(crate) trait Metric {
    /// Sorted latency samples in nanoseconds
    fn latencies_ns(&self) -> &[u64];

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
    pub fn from_results(results: &[&CaseResult]) -> Self {
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
    pub fn from_results(name: String, results: Vec<CaseResult>, duration: Duration) -> Self {
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

// ---------------------------------------------------------------------------
// Multi-round aggregation
// ---------------------------------------------------------------------------

/// Aggregated statistics across multiple benchmark rounds.
pub struct AggregatedSummary {
    pub name: String,
    pub rounds: usize,
    /// Per-round QPS values.
    pub qps_values: Vec<f64>,
    /// Per-round overall percentile values (keyed by percentile label).
    pub overall_pcts: AggregatedPercentiles,
    /// Per-round overall error rates.
    pub error_rates: Vec<f64>,
    /// Per-label aggregated percentiles.
    pub by_label: HashMap<ClassLabel, AggregatedPercentiles>,
    /// Per-label per-round error rates.
    pub label_error_rates: HashMap<ClassLabel, Vec<f64>>,
}

impl AggregatedSummary {
    pub fn from_rounds(name: &str, summaries: &[RunSummary]) -> Self {
        let rounds = summaries.len();
        let mut qps_values = Vec::with_capacity(rounds);
        let mut error_rates = Vec::with_capacity(rounds);
        let mut overall_pcts = AggregatedPercentiles::new();
        let mut by_label: HashMap<ClassLabel, AggregatedPercentiles> = HashMap::new();
        let mut label_error_rates: HashMap<ClassLabel, Vec<f64>> = HashMap::new();

        for s in summaries {
            qps_values.push(s.qps());
            error_rates.push(s.error_rate());
            AggregatedPercentiles::push_from(&mut overall_pcts, s);

            for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
                if let Some(ls) = s.by_label.get(&label) {
                    let pcts = by_label
                        .entry(label)
                        .or_insert_with(AggregatedPercentiles::new);
                    AggregatedPercentiles::push_from(pcts, ls);
                    label_error_rates
                        .entry(label)
                        .or_default()
                        .push(ls.error_rate());
                }
            }
        }

        Self {
            name: name.to_string(),
            rounds,
            qps_values,
            overall_pcts,
            error_rates,
            by_label,
            label_error_rates,
        }
    }
}

impl std::fmt::Display for AggregatedSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{} (aggregated over {} rounds)  qps={}  error%={}",
            self.name,
            self.rounds,
            fmt_mean_std(&self.qps_values),
            fmt_mean_std(&self.error_rates),
        )?;

        write!(f, "{}", RenderWrapper(self).render_table("overall"))?;

        for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
            if self.by_label.contains_key(&label) {
                writeln!(f)?;
                write!(
                    f,
                    "{}",
                    RenderWrapper((self, label)).render_table(label.as_str())
                )?;
            }
        }

        Ok(())
    }
}

/// Aggregated percentile samples across rounds.
#[derive(Default)]
pub struct AggregatedPercentiles {
    pub p50: Vec<f64>,
    pub p90: Vec<f64>,
    pub p95: Vec<f64>,
    pub p99: Vec<f64>,
    pub p999: Vec<f64>,
}

impl AggregatedPercentiles {
    pub fn new() -> Self {
        Self {
            p50: Vec::new(),
            p90: Vec::new(),
            p95: Vec::new(),
            p99: Vec::new(),
            p999: Vec::new(),
        }
    }

    pub fn push_from(pcts: &mut AggregatedPercentiles, m: impl Metric) {
        pcts.p50.push(m.percentile_ms(50.0));
        pcts.p90.push(m.percentile_ms(90.0));
        pcts.p95.push(m.percentile_ms(95.0));
        pcts.p99.push(m.percentile_ms(99.0));
        pcts.p999.push(m.percentile_ms(99.9));
    }
}
