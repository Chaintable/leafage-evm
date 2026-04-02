use std::io;
use std::path::PathBuf;

use crate::render::report::{CompareAggReport, CompareReport, Report, SummaryReport};
use crate::runner::export::{
    AggSummaryOutput, AggregatedOutput, BenchmarkOutput, RoundOutput, RunMetadata, SummaryOutput,
    VerboseOutput, VerboseRound,
};
use crate::runner::summary::{AggregatedSummary, RunSummary};
use crate::runner::{build_client, run_cases, CaseResult};
use crate::corpus::{Corpus, CorpusCase};
use anyhow::Result;
use jsonrpsee::http_client::HttpClient;
use std::time::Duration;

/// Options passed from CLI into the benchmark runner.
pub struct BenchConfig {
    pub requests: Option<usize>,
    pub shuffle_seed: Option<u64>,
    pub rounds: usize,
    pub output_dir: Option<PathBuf>,
    pub verbose: bool,
    pub target_url: String,
    pub compare_url: Option<String>,
    pub concurrency: usize,
    pub label_filter: Option<String>,
    pub corpus_cases: usize,
}

pub struct BenchRunner {
    target: HttpClient,
    compare: Option<HttpClient>,
    cfg: BenchConfig,
}

struct RoundResult {
    target: (Vec<CaseResult>, Duration),
    compare: Option<(Vec<CaseResult>, Duration)>,
}

impl BenchRunner {
    pub fn new(cfg: BenchConfig) -> Result<Self> {
        Ok(Self {
            target: build_client(cfg.target_url.as_str())?,
            compare: cfg
                .compare_url
                .as_ref()
                .map(|url| build_client(url.as_str()))
                .transpose()?,
            cfg,
        })
    }

    pub async fn run(&self, corpus: Corpus) -> Result<()> {
        let metadata: RunMetadata = (&self.cfg).into();
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
            if let Some(cs) = compare_summary {
                compare_summaries.push(cs);
            }
        }

        let compare_ref = if compare_summaries.is_empty() {
            None
        } else {
            Some(compare_summaries.as_slice())
        };
        self.render_final_report(&target_summaries, compare_ref)?;
        benchmark_output.aggregated =
            Self::build_aggregated_output(&target_summaries, compare_ref);

        self.write_benchmark_output(&benchmark_output, &verbose_rounds.into())
            .await?;

        Ok(())
    }

    async fn run_round(&self, cases: &[CorpusCase]) -> Result<RoundResult> {
        let total = self.resolve_total_requests();

        let target = run_cases(
            self.target.clone(),
            cases.to_vec(),
            self.cfg.concurrency,
            total,
        )
        .await?;

        let compare = if let Some(ref cmp) = self.compare {
            Some(
                run_cases(cmp.clone(), cases.to_vec(), self.cfg.concurrency, total).await?,
            )
        } else {
            None
        };

        Ok(RoundResult { target, compare })
    }

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
            if !dir.exists() {
                tokio::fs::create_dir_all(dir).await?;
            }
            crate::runner::export::write_outputs(dir, output, verbose, self.cfg.verbose).await?;
        }
        Ok(())
    }

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
            agg_target.render_report(w)?;
        }

        Ok(())
    }

    fn build_aggregated_output(
        target_summaries: &[RunSummary],
        compare_summaries: Option<&[RunSummary]>,
    ) -> Option<AggregatedOutput> {
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

