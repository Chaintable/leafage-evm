pub(crate) mod export;
pub(crate) mod render;
mod summary;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::bench_runner::export::{
    AggSummaryOutput, AggregatedOutput, BenchmarkOutput, RoundOutput, RunMetadata, SummaryOutput,
    VerboseOutput, VerboseRound,
};
use crate::bench_runner::render::report::{CompareAggReport, CompareReport, Report, SummaryReport};
use crate::bench_runner::summary::{AggregatedSummary, RunSummary};
use crate::corpus::Corpus;
use crate::corpus::{ClassLabel, CorpusCase};
use anyhow::Result;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::EthApiClient;
use leafage_evm_types::{BlockId, Bytes};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Debug)]
pub struct CaseResult {
    #[allow(dead_code)]
    pub case_id: String,
    pub label: ClassLabel,
    pub latency: Duration,
    pub outcome: Outcome,
}

pub type Outcome = std::result::Result<Bytes, jsonrpsee::core::client::error::Error>;

impl CaseResult {
    pub fn is_ok(&self) -> bool {
        match self.outcome {
            Ok(_) => true,
            Err(ref err) => match err {
                jsonrpsee::core::client::error::Error::Call(call_err) => {
                    let msg = call_err.message();
                    msg.contains("execution reverted") || msg.contains("Reverted")
                }
                _ => false,
            },
        }
    }
}

/// Options passed from CLI into the benchmark runner.
pub struct RunConfig {
    pub requests: Option<usize>,
    pub shuffle_seed: Option<u64>,
    pub rounds: usize,
    pub output_dir: Option<PathBuf>,
    pub verbose: bool,
    // metadata fields for JSON export
    pub target_url: String,
    pub compare_url: Option<String>,
    pub concurrency: usize,
    pub label_filter: Option<String>,
    pub corpus_cases: usize,
}

pub struct BenchRunner {
    /// Primary endpoint client (leafage-evm).
    target: HttpClient,
    /// Optional comparison endpoint client (geth).
    compare: Option<HttpClient>,
    cfg: RunConfig,
}

/// Result of a single benchmark round.
struct RoundResult {
    target: (Vec<CaseResult>, Duration),
    compare: Option<(Vec<CaseResult>, Duration)>,
}

impl BenchRunner {
    pub fn new(cfg: RunConfig) -> Result<Self> {
        let build = |url: &str| -> Result<HttpClient> {
            Ok(HttpClientBuilder::default()
                .request_timeout(Duration::from_secs(30))
                .build(url)?)
        };
        Ok(Self {
            target: build(cfg.target_url.as_str())?,
            compare: cfg
                .compare_url
                .as_ref()
                .map(|url| build(url.as_str()))
                .transpose()?,
            cfg,
        })
    }

    pub async fn run(&self, mut corpus: Corpus) -> Result<()> {
        let metadata: RunMetadata = (&self.cfg).into();

        if let Some(seed) = self.cfg.shuffle_seed {
            let mut rng = StdRng::seed_from_u64(seed);
            corpus.cases.shuffle(&mut rng);
        }

        let rounds = self.cfg.rounds;

        let mut verbose_rounds: Vec<VerboseRound> = Vec::with_capacity(rounds);
        let mut benchmark_output = BenchmarkOutput {
            metadata,
            rounds: Vec::with_capacity(rounds),
            aggregated: None,
        };

        let mut target_summaries = Vec::with_capacity(rounds);
        let mut compare_summaries: Vec<RunSummary> = Vec::with_capacity(rounds);

        for round in 1..=rounds {
            let round_result = self.run_round(&corpus.cases).await?;
            let (verbose, output, target_summary, compare_summary) =
                self.process_round(round, round_result)?;

            verbose_rounds.push(verbose);
            benchmark_output.rounds.push(output);
            target_summaries.push(target_summary);
            if let Some(compare_summary) = compare_summary {
                compare_summaries.push(compare_summary);
            }
        }

        let compare_ref = if compare_summaries.is_empty() {
            None
        } else {
            Some(compare_summaries.as_slice())
        };
        self.render_final_report(&target_summaries, compare_ref)?;
        benchmark_output.aggregated = Self::build_aggregated_output(&target_summaries, compare_ref);

        self.write_benchmark_output(&benchmark_output, &verbose_rounds.into())
            .await?;

        Ok(())
    }

    /// Run a single round against the target (and optionally the compare) endpoint.
    async fn run_round(&self, cases: &[CorpusCase]) -> Result<RoundResult> {
        let total_requests = self.resolve_total_requests();

        let target = run_cases(
            self.target.clone(),
            cases.to_vec(),
            self.cfg.concurrency,
            total_requests,
        )
        .await?;

        let compare = if let Some(ref cmp_client) = self.compare {
            Some(
                run_cases(
                    cmp_client.clone(),
                    cases.to_vec(),
                    self.cfg.concurrency,
                    total_requests,
                )
                .await?,
            )
        } else {
            None
        };

        Ok(RoundResult { target, compare })
    }

    /// Process raw round results into verbose output, round output, and summaries.
    fn process_round(
        &self,
        round: usize,
        result: RoundResult,
    ) -> Result<(VerboseRound, RoundOutput, RunSummary, Option<RunSummary>)> {
        let (target_results, target_duration) = result.target;

        let verbose_target = target_results.iter().map(Into::into).collect();
        let verbose_compare = result
            .compare
            .as_ref()
            .map(|(cr, _)| cr.iter().map(Into::into).collect());

        let verbose = VerboseRound {
            round,
            target: verbose_target,
            compare: verbose_compare,
        };

        let target_summary =
            RunSummary::from_results("target".into(), target_results, target_duration);

        let compare_summary = result
            .compare
            .map(|(cr, cd)| RunSummary::from_results("compare".into(), cr, cd));

        // Stream to stdout
        SummaryReport {
            name: "target",
            round,
            summary: &target_summary,
        }
        .render_report(&mut io::stdout())?;

        if let Some(ref cs) = compare_summary {
            SummaryReport {
                name: "compare",
                round,
                summary: cs,
            }
            .render_report(&mut io::stdout())?;
        }

        let output = RoundOutput {
            round,
            target: SummaryOutput::from(&target_summary),
            compare: compare_summary.as_ref().map(SummaryOutput::from),
        };

        Ok((verbose, output, target_summary, compare_summary))
    }

    fn resolve_total_requests(&self) -> usize {
        self.cfg.requests.unwrap_or(self.cfg.corpus_cases)
    }

    async fn write_benchmark_output(
        &self,
        output: &BenchmarkOutput,
        verbose: &VerboseOutput,
    ) -> Result<()> {
        if let Some(ref dir) = self.cfg.output_dir {
            export::write_outputs(dir, output, verbose, self.cfg.verbose).await?;
        }
        Ok(())
    }

    /// Render the final aggregated / compare report to stdout. Pure display, no data returned.
    fn render_final_report(
        &self,
        target_summaries: &[RunSummary],
        compare_summaries: Option<&[RunSummary]>,
    ) -> io::Result<()> {
        let has_compare = compare_summaries.is_some();
        let multi_round = self.cfg.rounds > 1;
        let w = &mut io::stdout();

        if !multi_round && has_compare {
            let report = CompareReport {
                target: &target_summaries[0],
                compare: &compare_summaries.unwrap()[0],
            };
            report.render_report(w)?;
            return Ok(());
        }

        if !multi_round {
            return Ok(());
        }

        let agg_target = AggregatedSummary::from_rounds("target", target_summaries);

        if let Some(cmp) = compare_summaries {
            let agg_compare = AggregatedSummary::from_rounds("compare", cmp);
            let report = CompareAggReport {
                target: &agg_target,
                compare: &agg_compare,
            };
            report.render_report(w)?;
        } else {
            // Multi-round target only
            agg_target.render_report(w)?;
        }

        Ok(())
    }

    /// Build the aggregated output for JSON export. Pure data transformation, no side effects.
    fn build_aggregated_output(
        target_summaries: &[RunSummary],
        compare_summaries: Option<&[RunSummary]>,
    ) -> Option<AggregatedOutput> {
        // Only produce aggregated output for multi-round runs
        if target_summaries.len() <= 1 {
            return None;
        }

        let agg_target = AggregatedSummary::from_rounds("target", target_summaries);

        let compare = compare_summaries.map(|cmp| {
            let agg_compare = AggregatedSummary::from_rounds("compare", cmp);
            AggSummaryOutput::from(&agg_compare)
        });

        Some(AggregatedOutput {
            target: AggSummaryOutput::from(&agg_target),
            compare,
        })
    }
}

/// Dispatch all cases to `client` with bounded concurrency.
async fn run_cases(
    client: HttpClient,
    cases: Vec<CorpusCase>,
    concurrency: usize,
    total_requests: usize,
) -> Result<(Vec<CaseResult>, Duration)> {
    let sem = Arc::new(Semaphore::new(concurrency));
    let mut set = JoinSet::new();
    let wall_start = Instant::now();

    for i in 0..total_requests {
        let case = &cases[i % cases.len()];
        let client = client.clone();
        let sem = Arc::clone(&sem);
        let case_id = case.case_id.clone();
        let label = case.classification.label;
        let request = case.request.clone();
        let block_id = BlockId::latest();

        set.spawn(async move {
            let _permit = sem.acquire_owned().await?;
            let start = Instant::now();
            let outcome = client.call(request, block_id, None, None).await;
            Ok::<CaseResult, anyhow::Error>(CaseResult {
                case_id,
                label,
                latency: start.elapsed(),
                outcome,
            })
        });
    }

    let mut results = Vec::with_capacity(total_requests);
    while let Some(res) = set.join_next().await {
        results.push(res??);
    }

    Ok((results, wall_start.elapsed()))
}
