pub(crate) mod render;
mod summary;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::bench_runner::render::report::{CompareAggReport, CompareReport, Report, SummaryReport};
use crate::bench_runner::summary::{AggregatedSummary, RunSummary};
use crate::corpus::Corpus;
use crate::corpus::{ClassLabel, CorpusCase};
use anyhow::{bail, Result};
use jsonrpsee::core::client::Error;
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
                Error::Call(call_err) => {
                    let msg = call_err.message();
                    msg.contains("execution reverted") || msg.contains("Reverted")
                }
                _ => false,
            },
        }
    }
}
pub struct BenchRunner {
    /// Primary endpoint client (leafage-evm).
    /// HttpClient is zero-copy clone internally, no Arc needed.
    target: HttpClient,
    /// Optional comparison endpoint client (geth).
    compare: Option<HttpClient>,
    concurrency: usize,
}

impl BenchRunner {
    pub fn new(target_url: &str, compare_url: Option<&str>, concurrency: usize) -> Result<Self> {
        let build = |url: &str| -> Result<HttpClient> {
            Ok(HttpClientBuilder::default()
                .request_timeout(Duration::from_secs(30))
                .build(url)?)
        };
        Ok(Self {
            target: build(target_url)?,
            compare: compare_url.map(build).transpose()?,
            concurrency,
        })
    }

    pub async fn run(
        &self,
        mut corpus: Corpus,
        requests: Option<usize>,
        shuffle_seed: Option<u64>,
        rounds: usize,
    ) -> Result<()> {
        if let Some(seed) = shuffle_seed {
            let mut rng = StdRng::seed_from_u64(seed);
            corpus.cases.shuffle(&mut rng);
        }

        if self.compare.is_some() {
            let mut target_summaries = Vec::with_capacity(rounds);
            let mut compare_summaries = Vec::with_capacity(rounds);
            let target_name = "target";
            let compare_name = "compare";

            for round in 1..=rounds {
                let (t, c) = self.run_compare(&corpus.cases, requests).await?;
                let target_report = SummaryReport {
                    name:target_name,
                    round,
                    summary: &t,
                };
                let report = SummaryReport {
                    name:compare_name,
                    round,
                    summary: &c,
                };
                println!("{}", target_report.render_report());
                println!("{}", report.render_report());
                target_summaries.push(t);
                compare_summaries.push(c);
            }

            if rounds > 1 {
                let agg_target = AggregatedSummary::from_rounds(target_name, &target_summaries);
                let agg_compare = AggregatedSummary::from_rounds(compare_name, &compare_summaries);
                let agg_report = CompareAggReport {
                    target: &agg_target,
                    compare: &agg_compare,
                };
                println!("{}", agg_report.render_report());
            } else {
                let report = CompareReport {
                    target: &target_summaries[0],
                    compare: &compare_summaries[0],
                };
                println!("{}", report.render_report());
            }
        } else {
            let name = "target";
            let mut summaries = Vec::with_capacity(rounds);

            for round in 1..=rounds {
                let summary = self.run_target(&corpus.cases, requests).await?;
                let report = SummaryReport {
                    name,
                    round,
                    summary: &summary,
                };
                println!("{}",report.render_report());
                summaries.push(summary);
            }

            if rounds > 1 {
                let agg = AggregatedSummary::from_rounds(name, &summaries);
                println!("{}", agg.render_report());
            }
        }
        Ok(())
    }

    /// Run all cases against the primary endpoint and return aggregated results.
    pub async fn run_target(
        &self,
        cases: &[CorpusCase],
        requests: Option<usize>,
    ) -> Result<RunSummary> {
        let total_requests = resolve_total_requests(cases.len(), requests)?;
        let (results, duration) = run_cases(
            self.target.clone(),
            cases.to_vec(),
            self.concurrency,
            total_requests,
        )
        .await?;
        Ok(RunSummary::from_results("target".into(), results, duration))
    }

    /// Run all cases against both endpoints **sequentially** and return both summaries.
    ///
    /// The target endpoint is benchmarked first, then the compare endpoint.
    /// This avoids resource contention on a single host that would skew
    /// latency measurements.  Returns `(target, compare)`.
    pub async fn run_compare(
        &self,
        cases: &[CorpusCase],
        requests: Option<usize>,
    ) -> Result<(RunSummary, RunSummary)> {
        let compare = self
            .compare
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("compare endpoint not configured"))?
            .clone();

        let total_requests = resolve_total_requests(cases.len(), requests)?;

        // --- target first ---
        println!("  benchmarking target endpoint ...");
        let (target_results, target_duration) = run_cases(
            self.target.clone(),
            cases.to_vec(),
            self.concurrency,
            total_requests,
        )
        .await?;

        // --- then compare ---
        println!("  benchmarking compare endpoint ...");
        let (compare_results, compare_duration) =
            run_cases(compare, cases.to_vec(), self.concurrency, total_requests).await?;

        Ok((
            RunSummary::from_results("target".into(), target_results, target_duration),
            RunSummary::from_results("compare".into(), compare_results, compare_duration),
        ))
    }
}

fn resolve_total_requests(cases_len: usize, requests: Option<usize>) -> Result<usize> {
    if cases_len == 0 {
        bail!("no corpus cases after filtering");
    }
    match requests {
        Some(0) => bail!("requests must be greater than 0"),
        Some(n) => Ok(n),
        None => Ok(cases_len),
    }
}

/// Dispatch all cases to `client` with bounded concurrency.
///
/// All tasks are queued upfront and up to `concurrency` run in parallel at
/// any time (Semaphore), so there is no chunk-level serialisation.
/// Returns `(case_results, wall_clock_duration)`.
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

        // All tasks are queued immediately.  Each task acquires the permit
        // itself, so the main loop is never blocked and true concurrency is
        // bounded by the semaphore without chunk-level serialisation.
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
