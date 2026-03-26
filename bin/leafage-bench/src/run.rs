use crate::runner::prepare_corpus;
use crate::runner::bench::{BenchConfig, BenchRunner};
use crate::runner::stress::{StressConfig, StressRunner};
use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::PathBuf;

/// Run benchmarks against one or two RPC endpoints.
#[derive(Debug, Args)]
pub struct Command {
    #[command(subcommand)]
    pub mode: RunMode,
}

#[derive(Debug, Subcommand)]
pub enum RunMode {
    /// Fixed-concurrency benchmark: run N rounds and report latency / QPS.
    Bench(BenchCommand),

    /// Stress-test: ramp concurrency to find the maximum sustainable QPS.
    Stress(StressCommand),
}

impl Command {
    pub async fn run(self) -> Result<()> {
        match self.mode {
            RunMode::Bench(cmd) => cmd.run().await,
            RunMode::Stress(cmd) => cmd.run().await,
        }
    }
}

// ─── common args ─────────────────────────────────────────────

/// Arguments shared by both bench and stress sub-commands.
#[derive(Debug, Args)]
pub struct CommonArgs {
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

    /// Total number of requests to send per endpoint per round.
    ///
    /// Defaults to the number of cases in the (filtered) corpus.
    #[arg(long, value_name = "N")]
    pub requests: Option<usize>,

    /// Number of benchmark rounds to run (per concurrency level for stress).
    #[arg(long, default_value_t = 1, value_name = "N")]
    pub rounds: usize,

    /// Shuffle seed for corpus ordering.
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,
}

impl CommonArgs {
    fn print_info(&self) {
        println!("corpus      : {}", self.corpus.display());
        println!("label       : {}", self.label.as_deref().unwrap_or("all"));
        println!("target      : {}", self.target);
        if let Some(ref c) = self.compare {
            println!("compare     : {c}");
        }
        println!(
            "requests    : {}",
            self.requests
                .map(|v| v.to_string())
                .unwrap_or_else(|| "auto".into())
        );
        println!("rounds      : {}", self.rounds);
        println!(
            "seed        : {}",
            self.seed
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".into())
        );
    }
}

// ─── bench ───────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct BenchCommand {
    #[command(flatten)]
    pub common: CommonArgs,

    /// Number of concurrent requests sent to each endpoint.
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub concurrency: usize,

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

impl BenchCommand {
    pub async fn run(self) -> Result<()> {
        self.common.print_info();
        println!("concurrency : {}", self.concurrency);
        if let Some(ref d) = self.output_dir {
            println!("output-dir  : {}", d.display());
            println!("verbose     : {}", self.verbose);
        }

        let corpus = prepare_corpus(
            self.common.corpus.as_path(),
            self.common.label.as_deref(),
            self.common.seed,
        )?;
        let effective = self.common.requests.unwrap_or(corpus.cases.len());
        println!("effective requests per round: {effective}\n");

        let cfg = BenchConfig {
            requests: self.common.requests,
            shuffle_seed: self.common.seed,
            rounds: self.common.rounds,
            output_dir: self.output_dir,
            verbose: self.verbose,
            target_url: self.common.target,
            compare_url: self.common.compare,
            concurrency: self.concurrency,
            label_filter: self.common.label,
            corpus_cases: corpus.cases.len(),
        };
        BenchRunner::new(cfg)?.run(corpus).await
    }
}

// ─── stress ──────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct StressCommand {
    #[command(flatten)]
    pub common: CommonArgs,

    /// Concurrency levels to test, specified as a comma-separated list.
    /// The stress test will run with each level in this list.
    #[arg(long, default_value = "100,200,500,1000,2000", value_delimiter = ',', value_name = "N,N,...")]
    pub concurrency_levels: Vec<usize>,

    /// Maximum allowable error rate (as a percentage) for the test to be considered successful.
    #[arg(long, default_value_t = 1.0, value_name = "PCT")]
    pub max_error_rate: f64,
}

impl StressCommand {
    pub async fn run(self) -> Result<()> {
        self.common.print_info();
        println!("concurrency-levels: {:?}", self.concurrency_levels);
        println!("max-error-rate    : {:.2}%", self.max_error_rate);
        println!();

        let corpus = prepare_corpus(
            self.common.corpus.as_path(),
            self.common.label.as_deref(),
            self.common.seed,
        )?;

        let cfg = StressConfig {
            target_url: self.common.target,
            compare_url: self.common.compare,
            concurrency_levels: self.concurrency_levels,
            requests: self.common.requests,
            rounds: self.common.rounds,
            max_error_rate: self.max_error_rate,
        };
        StressRunner::new(cfg)?.run(&corpus).await
    }
}
