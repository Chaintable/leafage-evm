use crate::bench_runner::render::table::{AggCompareView, CompareView};
use crate::bench_runner::render::{fmt_mean_std, TableView};
use crate::bench_runner::summary::{AggregatedSummary, Metric, RunSummary};
use crate::corpus::ClassLabel;
use std::fmt::Write;

pub(crate) trait Report {
    fn render_report(&self) -> String;
}

pub struct SummaryReport<'a> {
    pub name: &'a str,
    pub round: usize,
    pub summary: &'a RunSummary,
}

impl<'a> Report for SummaryReport<'a> {
    fn render_report(&self) -> String {
        let mut out = String::new();
        let round = self.round;
        let _ = writeln!(out, "\n── round {round} {} ──\n", self.name);
        let _ = writeln!(
            out,
            "{} total={} errors={} ({:.1}%) duration={:.2}s qps={:.1}",
            self.summary.name,
            self.summary.total,
            self.summary.errors,
            self.summary.error_rate(),
            self.summary.duration.as_secs_f64(),
            self.summary.qps()
        );
        let table = self.summary.render_overall_table("overall");
        let _ = write!(out, "{table}");
        out
    }
}

impl Report for AggregatedSummary {
    fn render_report(&self) -> String {
        let mut out = String::new();
        let rounds = self.rounds;
        let _ = writeln!(out, "\n══ aggregated ({rounds} rounds) ══\n");
        let _ = writeln!(
            out,
            "{} (aggregated over {} rounds)  qps={}  error%={}",
            self.name,
            self.rounds,
            fmt_mean_std(&self.qps_values),
            fmt_mean_std(&self.error_rates),
        );

        let _ = write!(out, "{}", self.render_overall_table("overall"));

        for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
            if let Some(label_table) = self.render_label_table(label) {
                let _ = writeln!(out);
                let _ = write!(out, "{}", label_table);
            }
        }
        out
    }
}

pub struct CompareReport<'a> {
    pub target: &'a RunSummary,
    pub compare: &'a RunSummary,
}

pub struct CompareAggReport<'a> {
    pub target: &'a AggregatedSummary,
    pub compare: &'a AggregatedSummary,
}

impl<'a> Report for CompareReport<'a> {
    fn render_report(&self) -> String {
        let (target, compare) = (self.target, self.compare);
        let mut out = String::new();
        let table_view = CompareView { target, compare };
        let _ = writeln!(&mut out, "{} vs {}", target.name, compare.name);

        let _ = writeln!(&mut out, "\n[overall]");
        let _ = writeln!(&mut out, "{}", table_view.render_overall_table("overall"));

        for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
            if !target.by_label.contains_key(&label) || !compare.by_label.contains_key(&label) {
                continue;
            }
            if let Some(label_table) = table_view.render_label_table(label) {
                let _ = writeln!(&mut out, "\n[{}]", label.as_str());
                let _ = writeln!(&mut out, "{}", label_table);
            }
        }

        out
    }
}

impl<'a> Report for CompareAggReport<'a> {
    fn render_report(&self) -> String {
        let (target, compare) = (self.target, self.compare);
        let mut out = String::new();
        let _ = writeln!(out, "\n══ aggregated ({} rounds) ══\n", self.target.rounds);
        let _ = writeln!(
            &mut out,
            "{} vs {} (aggregated over {} rounds)",
            target.name, compare.name, target.rounds
        );

        let table_view = AggCompareView { target, compare };
        let _ = writeln!(&mut out, "\n[overall]");
        let _ = writeln!(&mut out, "{}", table_view.render_overall_table("overall"));

        for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
            if let Some(label_table) = table_view.render_label_table(label) {
                let _ = writeln!(&mut out, "\n[{}]", label.as_str());
                let _ = writeln!(&mut out, "{}", label_table);
            }
        }
        out
    }
}
