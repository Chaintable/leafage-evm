use crate::render::{fmt_mean_std, format_delta_percent, mean, stddev};
use crate::runner::summary::{
    AggregatedPercentiles, AggregatedSummary, Metric, RunSummary, StressLevelResult,
};
use crate::corpus::ClassLabel;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{ContentArrangement, Table};

pub(crate) trait TableView {
    fn render_overall_table(&self, title: &str) -> Table;
    fn render_label_table(&self, label: ClassLabel) -> Option<Table>;
}

impl TableView for RunSummary {
    fn render_overall_table(&self, title: &str) -> Table {
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header([
                "label", "count", "error%", "p50 ms", "p95 ms", "p99 ms", "p999 ms",
            ]);

        table.add_row([
            title.to_string(),
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
        table
    }

    fn render_label_table(&self, _label: ClassLabel) -> Option<Table> {
        unimplemented!()
    }
}

pub(crate) struct CompareView<'a> {
    pub target: &'a RunSummary,
    pub compare: &'a RunSummary,
}

impl TableView for CompareView<'_> {
    fn render_overall_table(&self, title: &str) -> Table {
        render_compare_metric_table(title, self.target, self.compare)
    }

    fn render_label_table(&self, label: ClassLabel) -> Option<Table> {
        if let (Some(target), Some(compare)) = (
            self.target.by_label.get(&label),
            self.compare.by_label.get(&label),
        ) {
            return Some(render_compare_metric_table(label.as_str(), target, compare));
        }
        None
    }
}

fn render_compare_metric_table(title: &str, t: impl Metric, c: impl Metric) -> Table {
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

impl TableView for AggregatedSummary {
    fn render_overall_table(&self, title: &str) -> Table {
        render_aggregated_table(title, &self.overall_pcts, &self.error_rates)
    }

    fn render_label_table(&self, label: ClassLabel) -> Option<Table> {
        if let Some(pcts) = self.by_label.get(&label) {
            let err = self
                .label_error_rates
                .get(&label)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            return Some(render_aggregated_table(label.as_str(), pcts, err));
        }
        None
    }
}
fn render_aggregated_table(
    title: &str,
    pcts: &AggregatedPercentiles,
    error_rates: &[f64],
) -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header([title, "mean", "stddev"]);

    table.add_row([
        "error%",
        &format!("{:.2}", mean(error_rates)),
        &format!("{:.2}", stddev(error_rates)),
    ]);
    table.add_row([
        "p50 ms",
        &format!("{:.2}", mean(&pcts.p50)),
        &format!("{:.2}", stddev(&pcts.p50)),
    ]);
    table.add_row([
        "p90 ms",
        &format!("{:.2}", mean(&pcts.p90)),
        &format!("{:.2}", stddev(&pcts.p90)),
    ]);
    table.add_row([
        "p95 ms",
        &format!("{:.2}", mean(&pcts.p95)),
        &format!("{:.2}", stddev(&pcts.p95)),
    ]);
    table.add_row([
        "p99 ms",
        &format!("{:.2}", mean(&pcts.p99)),
        &format!("{:.2}", stddev(&pcts.p99)),
    ]);
    table.add_row([
        "p999 ms",
        &format!("{:.2}", mean(&pcts.p999)),
        &format!("{:.2}", stddev(&pcts.p999)),
    ]);
    table
}

pub(crate) struct AggCompareView<'a> {
    pub target: &'a AggregatedSummary,
    pub compare: &'a AggregatedSummary,
}

impl TableView for AggCompareView<'_> {
    fn render_overall_table(&self, title: &str) -> Table {
        render_aggregated_compare_table(
            title,
            &self.target.overall_pcts,
            &self.target.error_rates,
            &self.target.qps_values,
            &self.compare.overall_pcts,
            &self.compare.error_rates,
            &self.compare.qps_values,
        )
    }

    fn render_label_table(&self, label: ClassLabel) -> Option<Table> {
        if let (Some(tp), Some(cp)) = (
            self.target.by_label.get(&label),
            self.compare.by_label.get(&label),
        ) {
            let te = self
                .target
                .label_error_rates
                .get(&label)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let ce = self
                .compare
                .label_error_rates
                .get(&label)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            return Some(render_aggregated_compare_table(
                label.as_str(),
                tp,
                te,
                &[],
                cp,
                ce,
                &[],
            ));
        }
        None
    }
}

fn render_aggregated_compare_table(
    title: &str,
    tp: &AggregatedPercentiles,
    te: &[f64],
    tq: &[f64],
    cp: &AggregatedPercentiles,
    ce: &[f64],
    cq: &[f64],
) -> Table {
    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header([title, "target (mean±std)", "compare (mean±std)", "delta%"]);

    if !tq.is_empty() && !cq.is_empty() {
        table.add_row([
            "qps".to_string(),
            fmt_mean_std(tq),
            fmt_mean_std(cq),
            format_delta_percent(mean(tq), mean(cq)),
        ]);
    }

    table.add_row([
        "error%".to_string(),
        fmt_mean_std(te),
        fmt_mean_std(ce),
        format_delta_percent(mean(te), mean(ce)),
    ]);

    for (label, tv, cv) in [
        ("p50 ms", &tp.p50, &cp.p50),
        ("p90 ms", &tp.p90, &cp.p90),
        ("p95 ms", &tp.p95, &cp.p95),
        ("p99 ms", &tp.p99, &cp.p99),
        ("p999 ms", &tp.p999, &cp.p999),
    ] {
        table.add_row([
            label.to_string(),
            fmt_mean_std(tv),
            fmt_mean_std(cv),
            format_delta_percent(mean(tv), mean(cv)),
        ]);
    }

    table
}

/// Single-endpoint stress table view.
pub(crate) struct StressSingleView<'a> {
    pub levels: &'a [StressLevelResult],
}

impl TableView for StressSingleView<'_> {
    fn render_overall_table(&self, _title: &str) -> Table {
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header([
                "concurrency",
                "QPS (mean±std)",
                "error% (mean±std)",
                "p50 ms",
                "p95 ms",
                "p99 ms",
                "p999 ms",
                "status",
            ]);

        for level in self.levels {
            table.add_row([
                level.concurrency.to_string(),
                fmt_mean_std(&level.qps_values),
                fmt_mean_std(&level.error_rates),
                fmt_mean_std(&level.overall_pcts.p50),
                fmt_mean_std(&level.overall_pcts.p95),
                fmt_mean_std(&level.overall_pcts.p99),
                fmt_mean_std(&level.overall_pcts.p999),
                if level.breached {
                    "⚠ breached".into()
                } else {
                    "ok".into()
                },
            ]);
        }
        table
    }

    fn render_label_table(&self, _label: ClassLabel) -> Option<Table> {
        None
    }
}

/// Two-endpoint stress comparison table view.
pub(crate) struct StressCompareView<'a> {
    pub target: &'a [StressLevelResult],
    pub compare: &'a [StressLevelResult],
}

impl TableView for StressCompareView<'_> {
    fn render_overall_table(&self, _title: &str) -> Table {
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header([
                "concurrency",
                "endpoint",
                "QPS (mean±std)",
                "error% (mean±std)",
                "p50 ms",
                "p95 ms",
                "p99 ms",
                "p999 ms",
                "status",
            ]);

        let compare_by_c: std::collections::HashMap<usize, &StressLevelResult> =
            self.compare.iter().map(|l| (l.concurrency, l)).collect();

        for level in self.target {
            add_stress_row(&mut table, level, "target");

            if let Some(cmp) = compare_by_c.get(&level.concurrency) {
                add_stress_row(&mut table, cmp, "compare");
            }
        }

        // Compare levels that target didn't reach
        for level in self.compare {
            if !self
                .target
                .iter()
                .any(|t| t.concurrency == level.concurrency)
            {
                add_stress_row(&mut table, level, "compare");
            }
        }

        table
    }

    fn render_label_table(&self, _label: ClassLabel) -> Option<Table> {
        None
    }
}

/// Render a delta comparison table: one row per shared concurrency level.
///
/// All deltas use compare as the base. Positive value = target is better:
///   - QPS:            `(target − compare) / compare`  → higher target QPS = positive
///   - latency / err%: `(compare − target) / compare`  → lower target latency = positive
pub(crate) fn render_stress_delta_table(
    target: &[StressLevelResult],
    compare: &[StressLevelResult],
) -> Table {
    let compare_by_c: std::collections::HashMap<usize, &StressLevelResult> =
        compare.iter().map(|l| (l.concurrency, l)).collect();

    let mut table = Table::new();
    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header([
            "concurrency",
            "QPS Δ%",
            "error% Δ%",
            "p50 Δ%",
            "p95 Δ%",
            "p99 Δ%",
            "p999 Δ%",
        ]);

    for t in target {
        if let Some(c) = compare_by_c.get(&t.concurrency) {
            let t_qps = mean(&t.qps_values);
            let c_qps = mean(&c.qps_values);
            // QPS: higher is better  → (target − compare) / compare
            let qps_delta = format_delta_percent(c_qps, t_qps);
            // latency / error%: lower is better → (compare − target) / compare
            let err_delta = fmt_lower_is_better(mean(&c.error_rates), mean(&t.error_rates));
            let p50_delta = fmt_lower_is_better(mean(&c.overall_pcts.p50), mean(&t.overall_pcts.p50));
            let p95_delta = fmt_lower_is_better(mean(&c.overall_pcts.p95), mean(&t.overall_pcts.p95));
            let p99_delta = fmt_lower_is_better(mean(&c.overall_pcts.p99), mean(&t.overall_pcts.p99));
            let p999_delta = fmt_lower_is_better(mean(&c.overall_pcts.p999), mean(&t.overall_pcts.p999));

            table.add_row([
                t.concurrency.to_string(),
                qps_delta,
                err_delta,
                p50_delta,
                p95_delta,
                p99_delta,
                p999_delta,
            ]);
        }
    }

    table
}

/// For metrics where lower is better (latency, error%):
/// delta = (compare − target) / compare × 100
/// Positive result means target is better (lower value).
fn fmt_lower_is_better(compare_val: f64, target_val: f64) -> String {
    if compare_val.abs() < f64::EPSILON {
        return "-".to_string();
    }
    let delta = (compare_val - target_val) / compare_val * 100.0;
    format!("{:+.2}%", delta)
}

fn add_stress_row(table: &mut Table, level: &StressLevelResult, endpoint: &str) {
    table.add_row([
        level.concurrency.to_string(),
        endpoint.to_string(),
        fmt_mean_std(&level.qps_values),
        fmt_mean_std(&level.error_rates),
        fmt_mean_std(&level.overall_pcts.p50),
        fmt_mean_std(&level.overall_pcts.p95),
        fmt_mean_std(&level.overall_pcts.p99),
        fmt_mean_std(&level.overall_pcts.p999),
        if level.breached {
            "⚠ breached".into()
        } else {
            "ok".into()
        },
    ]);
}

