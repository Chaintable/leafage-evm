use crate::bench_runner::{BenchRunner, RunConfig};
use crate::corpus::ClassLabel;
use crate::corpus::Corpus;
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;
use std::str::FromStr;

/// Run the benchmark against one or two RPC endpoints.
#[derive(Debug, Args)]
pub struct Command {
    /// Path to the corpus JSON file.
    #[arg(long, short)]
    pub corpus: PathBuf,

    /// Only run cases with this complexity label.
    /// Omit to run all labels.
    #[arg(long, value_parser = ["L1", "L2", "L3"])]
    pub label: Option<String>,

    /// HTTP(S) URL of the primary RPC endpoint (leafage-evm).
    #[arg(long, value_name = "URL")]
    pub target: String,

    /// HTTP(S) URL of the comparison RPC endpoint (geth).
    #[arg(long, value_name = "URL")]
    pub compare: Option<String>,

    /// Number of concurrent requests sent to each endpoint.
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub concurrency: usize,

    /// Total number of requests to send per endpoint per round.
    ///
    /// Defaults to the number of cases in the (filtered) corpus.
    #[arg(long, value_name = "N")]
    pub requests: Option<usize>,

    /// Number of benchmark rounds to run.
    #[arg(long, default_value_t = 1, value_name = "N")]
    pub rounds: usize,

    /// Shuffle seed for corpus ordering.
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,

    /// Output directory for export files.
    ///
    /// Files written to this directory:
    ///   summary.json  — aggregated statistics for every round
    ///   verbose.json  — per-request details (only when --verbose is set)
    #[arg(long, value_name = "DIR")]
    pub output_dir: Option<PathBuf>,

    /// Include per-request details (return value, error, latency) in a
    /// separate verbose.json file.
    ///
    /// Only effective when --output-dir is set.
    #[arg(long, default_value_t = false)]
    pub verbose: bool,
}

impl Command {
    fn print_base_info(&self) {
        println!("corpus      : {}", self.corpus.display());
        println!("label       : {}", self.label.as_deref().unwrap_or("all"));
        println!("target      : {}", self.target);
        if let Some(ref cmp) = self.compare {
            println!("compare     : {cmp}");
        }
        println!("concurrency : {}", self.concurrency);
        println!(
            "requests    : {}",
            self.requests
                .map(|v| v.to_string())
                .unwrap_or_else(|| "auto(corpus size)".to_string())
        );
        println!("rounds      : {}", self.rounds);
        println!(
            "shuffle-seed: {}",
            self.seed
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".to_string())
        );
        if let Some(ref dir) = self.output_dir {
            println!("output-dir  : {}", dir.display());
            println!("verbose     : {}", self.verbose);
        }
    }

    pub async fn run(self) -> Result<()> {
        self.print_base_info();
        let label = self
            .label
            .as_ref()
            .and_then(|label| ClassLabel::from_str(label).ok());
        let mut corpus = Corpus::load(self.corpus.as_path())?;
        corpus.filter_label(label);

        let effective_requests = self.requests.unwrap_or(corpus.cases.len());
        println!("effective requests per round: {effective_requests}");
        println!();
        let cfg = RunConfig {
            requests: self.requests,
            shuffle_seed: self.seed,
            rounds: self.rounds,
            output_dir: self.output_dir,
            verbose: self.verbose,
            target_url: self.target,
            compare_url: self.compare,
            concurrency: self.concurrency,
            label_filter: self.label,
            corpus_cases: corpus.cases.len(),
        };
        let runner = BenchRunner::new(cfg)?;
        runner.run(corpus).await?;
        Ok(())
    }
}
