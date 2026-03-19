use crate::bench_runner::render::table::{AggCompareLabelView, AggCompareOverallView, CompareLabelView, CompareView};
use crate::bench_runner::render::TableView;
use crate::bench_runner::summary::{AggregatedSummary, RunSummary};
use crate::corpus::ClassLabel;
use std::fmt::Write;

pub(crate) trait Report {
    fn render_report(&self) -> String;
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
        let overall_view = CompareView { target, compare };
        let _ = writeln!(&mut out, "{} vs {}", target.name, compare.name);

        let _ = writeln!(&mut out, "\n[overall]");
        let _ = writeln!(&mut out, "{}", overall_view.render_table("overall"));

        for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
            if !target.by_label.contains_key(&label) || !compare.by_label.contains_key(&label) {
                continue;
            }
            let target = target.by_label.get(&label).unwrap();
            let compare = compare.by_label.get(&label).unwrap();

            let label_view = CompareLabelView { target, compare };
            let _ = writeln!(&mut out, "\n[{}]", label.as_str());
            let _ = writeln!(&mut out, "{}", label_view.render_table(label.as_str()));
        }

        out
    }
}

impl<'a> Report for CompareAggReport<'a> {
    fn render_report(&self) -> String {
        let (target, compare) = (self.target, self.compare);
        let mut out = String::new();
        let _ = writeln!(
            &mut out,
            "{} vs {} (aggregated over {} rounds)",
            target.name, compare.name, target.rounds
        );

        let overall_view = AggCompareOverallView { target, compare };
        let _ = writeln!(&mut out, "\n[overall]");
        let _ = writeln!(&mut out, "{}", overall_view.render_table("overall"));

        for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
            if !target.by_label.contains_key(&label) || !compare.by_label.contains_key(&label) {
                continue;
            }
            let label_view = AggCompareLabelView {
                target,
                compare,
                label,
            };
            let _ = writeln!(&mut out, "\n[{}]", label.as_str());
            let _ = writeln!(&mut out, "{}", label_view.render_table(label.as_str()));
        }
        out
    }
}
