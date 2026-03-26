use crate::render::{mean, stddev};
use crate::runner::summary::{
    AggregatedPercentiles, AggregatedSummary, LabelStats, Metric, RunSummary,
};
use crate::runner::bench::BenchConfig;
use crate::runner::CaseResult;
use crate::corpus::ClassLabel;
use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use tokio::io::AsyncWriteExt;

#[derive(Serialize, Default)]
pub struct BenchmarkOutput {
    pub metadata: RunMetadata,
    pub rounds: Vec<RoundOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregated: Option<AggregatedOutput>,
}

#[derive(Serialize, Default)]
pub struct RunMetadata {
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare: Option<String>,
    pub concurrency: usize,
    pub requests_per_round: usize,
    pub rounds: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shuffle_seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_filter: Option<String>,
    pub corpus_cases: usize,
}

impl From<&BenchConfig> for RunMetadata {
    fn from(cfg: &BenchConfig) -> Self {
        RunMetadata {
            target: cfg.target_url.to_string(),
            compare: cfg.compare_url.clone(),
            concurrency: cfg.concurrency,
            requests_per_round: cfg.requests.unwrap_or(cfg.corpus_cases),
            rounds: cfg.rounds,
            shuffle_seed: cfg.shuffle_seed,
            label_filter: cfg.label_filter.clone(),
            corpus_cases: cfg.corpus_cases,
        }
    }
}

#[derive(Serialize)]
pub struct RoundOutput {
    pub round: usize,
    pub target: SummaryOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare: Option<SummaryOutput>,
}

#[derive(Serialize)]
pub struct SummaryOutput {
    pub total: usize,
    pub errors: usize,
    pub duration_s: f64,
    pub qps: f64,
    pub error_rate: f64,
    pub latencies: PercentilesOutput,
    pub by_label: HashMap<String, LabelOutput>,
}

#[derive(Serialize)]
pub struct PercentilesOutput {
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub p999_ms: f64,
}

#[derive(Serialize)]
pub struct LabelOutput {
    pub total: usize,
    pub errors: usize,
    pub error_rate: f64,
    pub latencies: PercentilesOutput,
}

#[derive(Serialize)]
pub struct AggregatedOutput {
    pub target: AggSummaryOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare: Option<AggSummaryOutput>,
}

#[derive(Serialize)]
pub struct AggSummaryOutput {
    pub rounds: usize,
    pub qps_mean: f64,
    pub qps_stddev: f64,
    pub error_rate_mean: f64,
    pub error_rate_stddev: f64,
    pub latencies: AggPercentilesOutput,
    pub by_label: HashMap<String, AggLabelOutput>,
}

#[derive(Serialize)]
pub struct AggPercentilesOutput {
    pub p50_ms_mean: f64,
    pub p50_ms_stddev: f64,
    pub p90_ms_mean: f64,
    pub p90_ms_stddev: f64,
    pub p95_ms_mean: f64,
    pub p95_ms_stddev: f64,
    pub p99_ms_mean: f64,
    pub p99_ms_stddev: f64,
    pub p999_ms_mean: f64,
    pub p999_ms_stddev: f64,
}

#[derive(Serialize)]
pub struct AggLabelOutput {
    pub error_rate_mean: f64,
    pub error_rate_stddev: f64,
    pub latencies: AggPercentilesOutput,
}

#[derive(Serialize)]
pub struct VerboseOutput {
    pub rounds: Vec<VerboseRound>,
}

#[derive(Serialize)]
pub struct VerboseRound {
    pub round: usize,
    pub target: Vec<RequestOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare: Option<Vec<RequestOutput>>,
}

#[derive(Serialize)]
pub struct RequestOutput {
    pub case_id: String,
    pub label: String,
    pub latency_ms: f64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<Vec<VerboseRound>> for VerboseOutput {
    fn from(rounds: Vec<VerboseRound>) -> Self {
        Self { rounds }
    }
}
impl<F: Metric> From<F> for PercentilesOutput {
    fn from(m: F) -> Self {
        PercentilesOutput {
            p50_ms: m.percentile_ms(50.0),
            p90_ms: m.percentile_ms(90.0),
            p95_ms: m.percentile_ms(95.0),
            p99_ms: m.percentile_ms(99.0),
            p999_ms: m.percentile_ms(99.9),
        }
    }
}

impl From<&LabelStats> for LabelOutput {
    fn from(ls: &LabelStats) -> Self {
        Self {
            total: ls.total,
            errors: ls.errors,
            error_rate: ls.error_rate(),
            latencies: ls.into(),
        }
    }
}

impl From<&CaseResult> for RequestOutput {
    fn from(r: &CaseResult) -> Self {
        let (ok, result, error) = match &r.outcome {
            Ok(bytes) => (true, Some(bytes.to_string()), None),
            Err(err) => (r.is_ok(), None, Some(format!("{err}"))),
        };
        RequestOutput {
            case_id: r.case_id.clone(),
            label: r.label.as_str().to_string(),
            latency_ms: r.latency.as_nanos() as f64 / 1_000_000.0,
            ok,
            result,
            error,
        }
    }
}

impl From<&RunSummary> for SummaryOutput {
    fn from(s: &RunSummary) -> Self {
        let by_label = [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3]
            .into_iter()
            .filter_map(|label| {
                s.by_label
                    .get(&label)
                    .map(|ls| (label.as_str().to_string(), LabelOutput::from(ls)))
            })
            .collect();

        Self {
            total: s.total,
            errors: s.errors,
            duration_s: s.duration.as_secs_f64(),
            qps: s.qps(),
            error_rate: s.error_rate(),
            latencies: s.into(),
            by_label,
        }
    }
}

impl From<&AggregatedPercentiles> for AggPercentilesOutput {
    fn from(p: &AggregatedPercentiles) -> Self {
        Self {
            p50_ms_mean: mean(&p.p50),
            p50_ms_stddev: stddev(&p.p50),
            p90_ms_mean: mean(&p.p90),
            p90_ms_stddev: stddev(&p.p90),
            p95_ms_mean: mean(&p.p95),
            p95_ms_stddev: stddev(&p.p95),
            p99_ms_mean: mean(&p.p99),
            p99_ms_stddev: stddev(&p.p99),
            p999_ms_mean: mean(&p.p999),
            p999_ms_stddev: stddev(&p.p999),
        }
    }
}

impl From<&AggregatedSummary> for AggSummaryOutput {
    fn from(agg: &AggregatedSummary) -> Self {
        let by_label = [ClassLabel::L1, ClassLabel::L2, ClassLabel::L3]
            .into_iter()
            .filter_map(|label| {
                agg.by_label.get(&label).map(|pcts| {
                    let err = agg
                        .label_error_rates
                        .get(&label)
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    (
                        label.as_str().to_string(),
                        AggLabelOutput {
                            error_rate_mean: mean(err),
                            error_rate_stddev: stddev(err),
                            latencies: AggPercentilesOutput::from(pcts),
                        },
                    )
                })
            })
            .collect();

        Self {
            rounds: agg.rounds,
            qps_mean: mean(&agg.qps_values),
            qps_stddev: stddev(&agg.qps_values),
            error_rate_mean: mean(&agg.error_rates),
            error_rate_stddev: stddev(&agg.error_rates),
            latencies: AggPercentilesOutput::from(&agg.overall_pcts),
            by_label,
        }
    }
}

async fn write_file<T: ?Sized + Serialize>(dir: &Path, file_name: &str, value: &T) -> Result<()> {
    let path = dir.join(file_name);
    let mut file = tokio::fs::File::create(&path).await?;
    let json = serde_json::to_vec_pretty(value)?;
    file.write_all(json.as_ref()).await?;
    println!("wrote {}", path.display());
    Ok(())
}

async fn write_summary(dir: &Path, output: &BenchmarkOutput) -> Result<()> {
    write_file(dir, "summary.json", output).await
}

async fn write_verbose(dir: &Path, verbose: &VerboseOutput) -> Result<()> {
    write_file(dir, "verbose.json", verbose).await
}

pub async fn write_outputs(
    dir: &Path,
    summary: &BenchmarkOutput,
    verbose_output: &VerboseOutput,
    verbose: bool,
) -> Result<()> {
    write_summary(dir, summary).await?;
    if verbose {
        write_verbose(dir, verbose_output).await?;
    }
    Ok(())
}
