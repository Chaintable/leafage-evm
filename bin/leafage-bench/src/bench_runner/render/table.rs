use crate::bench_runner::render::{fmt_mean_std, format_delta_percent, mean, stddev};
use crate::bench_runner::summary::{AggregatedPercentiles, AggregatedSummary, Metric, RunSummary};
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
