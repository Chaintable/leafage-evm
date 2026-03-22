use crate::bench_runner::render::table::{AggCompareView, CompareView};
use crate::bench_runner::render::{fmt_mean_std, TableView};
use crate::bench_runner::summary::{AggregatedSummary, Metric, RunSummary};
use crate::corpus::ClassLabel;
use std::io::{self, Write};

pub(crate) trait Report {
    fn render_report(&self, w: &mut dyn Write) -> io::Result<()>;
}

pub struct SummaryReport<'a> {
    pub name: &'a str,
    pub round: usize,
    pub summary: &'a RunSummary,
}

impl<'a> Report for SummaryReport<'a> {
    fn render_report(&self, w: &mut dyn Write) -> io::Result<()> {
        let round = self.round;
        writeln!(w, "\n── round {round} {} ──\n", self.name)?;
        writeln!(
            w,
            "{} total={} errors={} ({:.1}%) duration={:.2}s qps={:.1}",
            self.summary.name,
            self.summary.total,
            self.summary.errors,
            self.summary.error_rate(),
            self.summary.duration.as_secs_f64(),
            self.summary.qps()
        )?;
        let table = self.summary.render_overall_table("overall");
        write!(w, "{table}")?;
        Ok(())
    }
}

impl Report for AggregatedSummary {
    fn render_report(&self, w: &mut dyn Write) -> io::Result<()> {
        let rounds = self.rounds;
        writeln!(w, "\n══ aggregated ({rounds} rounds) ══\n")?;
        writeln!(
            w,
            "{} (aggregated over {} rounds)  qps={}  error%={}",
            self.name,
            self.rounds,
            fmt_mean_std(&self.qps_values),
            fmt_mean_std(&self.error_rates),
        )?;

        write!(w, "{}", self.render_overall_table("overall"))?;

        for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
            if let Some(label_table) = self.render_label_table(label) {
                writeln!(w)?;
                write!(w, "{label_table}")?;
            }
        }
        Ok(())
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
    fn render_report(&self, w: &mut dyn Write) -> io::Result<()> {
        let (target, compare) = (self.target, self.compare);
        let table_view = CompareView { target, compare };
        writeln!(w, "{} vs {}", target.name, compare.name)?;

        writeln!(w, "\n[overall]")?;
        writeln!(w, "{}", table_view.render_overall_table("overall"))?;

        for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
            if let Some(label_table) = table_view.render_label_table(label) {
                writeln!(w, "\n[{}]", label.as_str())?;
                writeln!(w, "{label_table}")?;
            }
        }
        Ok(())
    }
}

impl<'a> Report for CompareAggReport<'a> {
    fn render_report(&self, w: &mut dyn Write) -> io::Result<()> {
        let (target, compare) = (self.target, self.compare);
        writeln!(w, "\n══ aggregated ({} rounds) ══\n", self.target.rounds)?;
        writeln!(
            w,
            "{} vs {} (aggregated over {} rounds)",
            target.name, compare.name, target.rounds
        )?;

        let table_view = AggCompareView { target, compare };
        writeln!(w, "\n[overall]")?;
        writeln!(w, "{}", table_view.render_overall_table("overall"))?;

        for label in [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3] {
            if let Some(label_table) = table_view.render_label_table(label) {
                writeln!(w, "\n[{}]", label.as_str())?;
                writeln!(w, "{label_table}")?;
            }
        }
        Ok(())
    }
}
