use crate::render::table::{
    AggCompareView, CompareView, StressCompareView, StressSingleView,
    render_stress_delta_table,
};
use crate::render::{fmt_mean_std, mean, TableView};
use crate::runner::summary::{AggregatedSummary, Metric, RunSummary, StressLevelResult};
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

/// Per-level report: renders a single concurrency level as a table row.
pub struct StressLevelReport<'a> {
    pub name: &'a str,
    pub level: &'a StressLevelResult,
}

/// Report for a stress test with a single endpoint.
pub struct StressReport<'a> {
    pub levels: &'a [StressLevelResult],
    pub name: &'a str,
}

/// Report for a stress test comparing two endpoints.
pub struct StressCompareReport<'a> {
    pub target: &'a [StressLevelResult],
    pub compare: &'a [StressLevelResult],
}

impl Report for StressLevelReport<'_> {
    fn render_report(&self, w: &mut dyn Write) -> io::Result<()> {
        let view = StressSingleView {
            levels: std::slice::from_ref(self.level),
        };
        writeln!(w, "\n── {} concurrency={} ──\n", self.name, self.level.concurrency)?;
        writeln!(w, "{}", view.render_overall_table(""))?;
        Ok(())
    }
}

impl Report for StressReport<'_> {
    fn render_report(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "\n══ stress test summary ({}) ══\n", self.name)?;

        let view = StressSingleView {
            levels: self.levels,
        };
        writeln!(w, "{}", view.render_overall_table(""))?;

        render_max_qps(w, self.levels, self.name)?;
        Ok(())
    }
}

impl Report for StressCompareReport<'_> {
    fn render_report(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "\n══ stress test summary (target vs compare) ══\n")?;

        let view = StressCompareView {
            target: self.target,
            compare: self.compare,
        };
        writeln!(w, "{}", view.render_overall_table(""))?;

        render_max_qps(w, self.target, "target")?;
        render_max_qps(w, self.compare, "compare")?;

        writeln!(w, "\n── delta (target vs compare, base=compare) ──\n")?;
        writeln!(
            w,
            "+N% = target is better (higher QPS / lower latency & error%)\n"
        )?;
        writeln!(w, "{}", render_stress_delta_table(self.target, self.compare))?;

        Ok(())
    }
}

fn render_max_qps(
    w: &mut dyn Write,
    levels: &[StressLevelResult],
    name: &str,
) -> io::Result<()> {
    let best = levels
        .iter()
        .filter(|l| !l.breached)
        .max_by(|a, b| {
            mean(&a.qps_values)
                .partial_cmp(&mean(&b.qps_values))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

    if let Some(best) = best {
        writeln!(
            w,
            "🏆 {name} max sustainable QPS: {:.1} (at concurrency={}, error%={:.2}%, p50={:.2}ms, p99={:.2}ms)",
            mean(&best.qps_values),
            best.concurrency,
            mean(&best.error_rates),
            mean(&best.overall_pcts.p50),
            mean(&best.overall_pcts.p99),
        )?;
    } else {
        writeln!(
            w,
            "⚠ {name}: all concurrency levels exceeded the error-rate threshold.",
        )?;
    }
    Ok(())
}
