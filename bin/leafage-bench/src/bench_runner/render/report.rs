use crate::bench_runner::render::Render;
use crate::bench_runner::render::table::RenderWrapper;
use crate::bench_runner::summary::{AggregatedSummary, RunSummary};
use crate::corpus::ClassLabel;
use std::fmt::Write;

pub (crate) trait Report {
    fn render_report(&self) -> String;
}

impl Report for (&RunSummary, &RunSummary) {
    fn render_report(&self) -> String {
        let (target, compare) = (self.0, self.1);
        let mut out = String::new();
        let _ = writeln!(&mut out, "{} vs {}", target.name, compare.name);

        let _ = writeln!(&mut out, "\n[overall]");
        let _ = writeln!(&mut out, "{}", (target, compare).render_table("overall"));

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
                (target, compare).render_table(label.as_str())
            );
        }

        out
    }
}

impl Report for (&AggregatedSummary, &AggregatedSummary) {
    fn render_report(&self) -> String {
        let (target, compare) = (self.0, self.1);
        let mut out = String::new();
        let _ = writeln!(
            &mut out,
            "{} vs {} (aggregated over {} rounds)",
            target.name, compare.name, target.rounds
        );

        let _ = writeln!(&mut out, "\n[overall]");
        let _ = writeln!(
            &mut out,
            "{}",
            RenderWrapper((target, compare)).render_table("overall")
        );

        for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
            if !target.by_label.contains_key(&label) || !compare.by_label.contains_key(&label) {
                continue;
            }
            let _ = writeln!(&mut out, "\n[{}]", label.as_str());
            let _ = writeln!(
                &mut out,
                "{}",
                RenderWrapper((target, compare, label)).render_table(label.as_str())
            );
        }
        out
    }
}