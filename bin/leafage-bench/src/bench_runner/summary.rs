use crate::bench_runner::CaseResult;
use crate::corpus::ClassLabel;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal Metric impl for testing pure functions without needing
    /// CaseResult / RPC types.
    struct FakeMetric {
        latencies_ns: Vec<u64>,
        total: usize,
        errors: usize,
    }

    impl FakeMetric {
        fn new(latencies_ms: &[f64], errors: usize) -> Self {
            let mut latencies_ns: Vec<u64> = latencies_ms
                .iter()
                .map(|ms| (*ms * 1_000_000.0) as u64)
                .collect();
            latencies_ns.sort_unstable();
            let total = latencies_ns.len();
            Self {
                latencies_ns,
                total,
                errors,
            }
        }
    }

    impl Metric for &FakeMetric {
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

    #[test]
    fn nearest_rank_single_element() {
        // Any percentile on a 1-element array should return index 0.
        assert_eq!(<&FakeMetric as Metric>::nearest_rank_index(1, 0.0), 0);
        assert_eq!(<&FakeMetric as Metric>::nearest_rank_index(1, 50.0), 0);
        assert_eq!(<&FakeMetric as Metric>::nearest_rank_index(1, 100.0), 0);
    }

    #[test]
    fn nearest_rank_boundary_zero() {
        assert_eq!(<&FakeMetric as Metric>::nearest_rank_index(10, 0.0), 0);
        assert_eq!(<&FakeMetric as Metric>::nearest_rank_index(10, -5.0), 0);
    }

    #[test]
    fn nearest_rank_boundary_hundred() {
        assert_eq!(<&FakeMetric as Metric>::nearest_rank_index(10, 100.0), 9);
        assert_eq!(<&FakeMetric as Metric>::nearest_rank_index(10, 150.0), 9);
    }

    #[test]
    fn nearest_rank_nan_inf() {
        // All non-finite values are treated as out-of-range and clamped to index 0.
        assert_eq!(<&FakeMetric as Metric>::nearest_rank_index(5, f64::NAN), 0);
        assert_eq!(
            <&FakeMetric as Metric>::nearest_rank_index(5, f64::INFINITY),
            0
        );
        assert_eq!(
            <&FakeMetric as Metric>::nearest_rank_index(5, f64::NEG_INFINITY),
            0
        );
    }

    #[test]
    fn nearest_rank_p50_ten_elements() {
        // len=10, pct=50 → rank = ceil(0.5 * 10) = 5 → index = 4
        assert_eq!(<&FakeMetric as Metric>::nearest_rank_index(10, 50.0), 4);
    }

    #[test]
    fn nearest_rank_p99_hundred_elements() {
        // len=100, pct=99 → rank = ceil(0.99 * 100) = 99 → index = 98
        assert_eq!(<&FakeMetric as Metric>::nearest_rank_index(100, 99.0), 98);
    }

    #[test]
    fn nearest_rank_p95_twenty_elements() {
        // len=20, pct=95 → rank = ceil(0.95 * 20) = ceil(19.0) = 19 → index = 18
        assert_eq!(<&FakeMetric as Metric>::nearest_rank_index(20, 95.0), 18);
    }

    #[test]
    fn percentile_ms_empty() {
        let m = FakeMetric::new(&[], 0);
        assert_eq!((&m).percentile_ms(50.0), 0.0);
    }

    #[test]
    fn percentile_ms_single() {
        let m = FakeMetric::new(&[10.0], 0);
        let p = (&m).percentile_ms(50.0);
        assert!((p - 10.0).abs() < 0.01);
    }

    #[test]
    fn percentile_ms_known() {
        // 10 elements: 1..=10 ms, sorted
        let latencies: Vec<f64> = (1..=10).map(|i| i as f64).collect();
        let m = FakeMetric::new(&latencies, 0);
        // p50 → index 4 → 5 ms
        let p50 = (&m).percentile_ms(50.0);
        assert!((p50 - 5.0).abs() < 0.01);
        // p100 → index 9 → 10 ms
        let p100 = (&m).percentile_ms(100.0);
        assert!((p100 - 10.0).abs() < 0.01);
    }

    #[test]
    fn error_rate_zero_total() {
        let m = FakeMetric::new(&[], 0);
        assert_eq!((&m).error_rate(), 0.0);
    }

    #[test]
    fn error_rate_no_errors() {
        let m = FakeMetric::new(&[1.0, 2.0, 3.0], 0);
        assert_eq!((&m).error_rate(), 0.0);
    }

    #[test]
    fn error_rate_half() {
        let m = FakeMetric {
            latencies_ns: vec![1_000_000, 2_000_000],
            total: 2,
            errors: 1,
        };
        assert!(((&m).error_rate() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn error_rate_all() {
        let m = FakeMetric {
            latencies_ns: vec![1_000_000],
            total: 1,
            errors: 1,
        };
        assert!(((&m).error_rate() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn push_from_collects_percentiles() {
        let m1 = FakeMetric::new(&[10.0, 20.0, 30.0, 40.0, 50.0], 0);
        let m2 = FakeMetric::new(&[100.0, 200.0, 300.0, 400.0, 500.0], 0);

        let mut pcts = AggregatedPercentiles::new();
        AggregatedPercentiles::push_from(&mut pcts, &m1);
        AggregatedPercentiles::push_from(&mut pcts, &m2);

        assert_eq!(pcts.p50.len(), 2);
        assert_eq!(pcts.p90.len(), 2);
        assert_eq!(pcts.p95.len(), 2);
        assert_eq!(pcts.p99.len(), 2);
        assert_eq!(pcts.p999.len(), 2);

        // m1 p50 → index 2 → 30ms, m2 p50 → index 2 → 300ms
        assert!((pcts.p50[0] - 30.0).abs() < 0.01);
        assert!((pcts.p50[1] - 300.0).abs() < 0.01);
    }

    #[test]
    fn push_from_empty_metric() {
        let m = FakeMetric::new(&[], 0);
        let mut pcts = AggregatedPercentiles::new();
        AggregatedPercentiles::push_from(&mut pcts, &m);

        // Empty metric → percentile_ms returns 0.0
        assert_eq!(pcts.p50.len(), 1);
        assert_eq!(pcts.p50[0], 0.0);
    }
    #[test]
    fn aggregated_percentiles_default_is_empty() {
        let pcts = AggregatedPercentiles::default();
        assert!(pcts.p50.is_empty());
        assert!(pcts.p90.is_empty());
        assert!(pcts.p95.is_empty());
        assert!(pcts.p99.is_empty());
        assert!(pcts.p999.is_empty());
    }
}
