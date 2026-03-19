use crate::bench_runner::render::{fmt_mean_std, format_delta_percent, mean, stddev};
use crate::bench_runner::summary::{
    AggregatedPercentiles, AggregatedSummary, LabelStats, Metric, RunSummary,
};
use crate::corpus::ClassLabel;
use comfy_table::presets::UTF8_FULL;
use comfy_table::{ContentArrangement, Table};

pub(crate) trait TableView {
    fn render_table(&self, title: &str) -> Table;
}

/// Compare view for two single-round summaries (overall).
pub(crate) struct CompareView<'a> {
    pub target: &'a RunSummary,
    pub compare: &'a RunSummary,
}

/// Compare view for two single-round label stats.
pub(crate) struct CompareLabelView<'a> {
    pub target: &'a LabelStats,
    pub compare: &'a LabelStats,
}

impl TableView for CompareView<'_> {
    fn render_table(&self, title: &str) -> Table {
        render_compare_metric_table(title, self.target, self.compare)
    }
}

impl TableView for CompareLabelView<'_> {
    fn render_table(&self, title: &str) -> Table {
        render_compare_metric_table(title, self.target, self.compare)
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
/// Aggregated overall view for a single endpoint across multiple rounds.
pub(crate) struct AggOverallView<'a> {
    pub summary: &'a AggregatedSummary,
}

/// Aggregated per-label view for a single endpoint across multiple rounds.
pub(crate) struct AggLabelView<'a> {
    pub summary: &'a AggregatedSummary,
    pub label: ClassLabel,
}

impl TableView for AggOverallView<'_> {
    fn render_table(&self, title: &str) -> Table {
        render_aggregated_table(title, &self.summary.overall_pcts, &self.summary.error_rates)
    }
}

impl TableView for AggLabelView<'_> {
    fn render_table(&self, title: &str) -> Table {
        let pcts = self.summary.by_label.get(&self.label).unwrap();
        let err = self
            .summary
            .label_error_rates
            .get(&self.label)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        render_aggregated_table(title, pcts, err)
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

/// Aggregated compare overall view: target vs compare across multiple rounds.
pub(crate) struct AggCompareOverallView<'a> {
    pub target: &'a AggregatedSummary,
    pub compare: &'a AggregatedSummary,
}

/// Aggregated compare per-label view: target vs compare for a specific label.
pub(crate) struct AggCompareLabelView<'a> {
    pub target: &'a AggregatedSummary,
    pub compare: &'a AggregatedSummary,
    pub label: ClassLabel,
}

impl TableView for AggCompareOverallView<'_> {
    fn render_table(&self, title: &str) -> Table {
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
}

impl TableView for AggCompareLabelView<'_> {
    fn render_table(&self, title: &str) -> Table {
        let tp = self.target.by_label.get(&self.label).unwrap();
        let cp = self.compare.by_label.get(&self.label).unwrap();
        let te = self
            .target
            .label_error_rates
            .get(&self.label)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let ce = self
            .compare
            .label_error_rates
            .get(&self.label)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        render_aggregated_compare_table(title, tp, te, &[], cp, ce, &[])
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
