use crate::render::mean;
use crate::render::report::{Report, StressCompareReport, StressLevelReport, StressReport};
use crate::runner::{build_client, run_cases};
use crate::runner::summary::{AggregatedSummary, RunSummary, StressLevelResult};
use crate::corpus::Corpus;
use anyhow::Result;
use jsonrpsee::http_client::HttpClient;
use std::io;

/// Options passed from CLI into the stress runner.
pub struct StressConfig {
    pub target_url: String,
    pub compare_url: Option<String>,
    pub concurrency_levels: Vec<usize>,
    pub requests: Option<usize>,
    pub rounds: usize,
    pub max_error_rate: f64,
}

pub struct StressRunner {
    target: HttpClient,
    compare: Option<HttpClient>,
    cfg: StressConfig,
}

impl StressRunner {
    pub fn new(cfg: StressConfig) -> Result<Self> {
        Ok(Self {
            target: build_client(&cfg.target_url)?,
            compare: cfg
                .compare_url
                .as_ref()
                .map(|url| build_client(url))
                .transpose()?,
            cfg,
        })
    }

    pub async fn run(&self, corpus: &Corpus) -> Result<()> {
        let requests = self.cfg.requests.unwrap_or(corpus.cases.len());

        let mut target_results: Vec<StressLevelResult> = Vec::new();
        let mut compare_results: Vec<StressLevelResult> = Vec::new();

        let mut target_stopped = false;
        let mut compare_stopped = false;

        for &concurrency in &self.cfg.concurrency_levels {
            if target_stopped && (self.compare.is_none() || compare_stopped) {
                break;
            }

            if !target_stopped {
                let level = Self::run_level(
                    "target", &self.target, corpus, concurrency, requests, self.cfg.rounds,
                ).await?;
                let breached = mean(&level.error_rates) > self.cfg.max_error_rate;
                let level = StressLevelResult { breached, ..level };
                StressLevelReport { name: "target", level: &level }
                    .render_report(&mut io::stdout())?;
                if breached {
                    target_stopped = true;
                }
                target_results.push(level);
            }

            if let Some(ref cmp) = self.compare {
                if !compare_stopped {
                    let level = Self::run_level(
                        "compare", cmp, corpus, concurrency, requests, self.cfg.rounds,
                    ).await?;
                    let breached = mean(&level.error_rates) > self.cfg.max_error_rate;
                    let level = StressLevelResult { breached, ..level };
                    StressLevelReport { name: "compare", level: &level }
                        .render_report(&mut io::stdout())?;
                    if breached {
                        compare_stopped = true;
                    }
                    compare_results.push(level);
                }
            }
        }

        let w = &mut io::stdout();
        if compare_results.is_empty() {
            StressReport {
                levels: &target_results,
                name: "target",
            }
            .render_report(w)?;
        } else {
            StressCompareReport {
                target: &target_results,
                compare: &compare_results,
            }
            .render_report(w)?;
        }

        Ok(())
    }

    async fn run_level(
        name: &str,
        client: &HttpClient,
        corpus: &Corpus,
        concurrency: usize,
        requests: usize,
        rounds: usize,
    ) -> Result<StressLevelResult> {
        let mut summaries: Vec<RunSummary> = Vec::with_capacity(rounds);

        for _ in 0..rounds {
            let (results, duration) =
                run_cases(client.clone(), corpus.cases.clone(), concurrency, requests).await?;
            let summary = RunSummary::from_results(name.to_string(), results, duration);
            summaries.push(summary);
        }

        let agg = AggregatedSummary::from_rounds(name, &summaries);

        Ok(StressLevelResult {
            concurrency,
            qps_values: agg.qps_values,
            error_rates: agg.error_rates,
            overall_pcts: agg.overall_pcts,
            breached: false,
        })
    }
}
