use crate::corpus::ClassLabel;
use crate::corpus::Corpus;
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;
use std::str::FromStr;
use crate::bench_runner::BenchRunner;

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
    ///
    /// Measures latency distribution (p50/p95/p99/p999), throughput (QPS),
    /// and error rate for the target endpoint. When --compare is omitted,
    /// only this endpoint is benchmarked to establish a performance baseline.
    #[arg(long, value_name = "URL")]
    pub target: String,

    /// HTTP(S) URL of the comparison RPC endpoint (geth).
    ///
    /// When provided, every case is replayed against both endpoints and the
    /// results are compared side-by-side (latency delta, QPS ratio, error rate).
    #[arg(long, value_name = "URL")]
    pub compare: Option<String>,

    /// Number of concurrent requests sent to each endpoint.
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub concurrency: usize,

    /// Total number of requests to send per endpoint.
    ///
    /// Defaults to the number of cases in the (filtered) corpus.
    /// Set to a larger value to replay the corpus in a round-robin loop.
    #[arg(long, value_name = "N")]
    pub requests: Option<usize>,

    /// Shuffle seed for corpus ordering.
    ///
    /// When set, corpus shuffle is deterministic and reproducible.
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,

    /// Directory for benchmark result files.
    ///
    /// A timestamped sub-directory is created automatically under this path,
    /// e.g. `bench-results/2026-03-17T12:00:00/`.
    #[arg(long, default_value = "bench-results", value_name = "DIR")]
    pub output_dir: PathBuf,

    /// Print one JSON line per request to stdout in addition to the summary.
    #[arg(long, default_value_t = false)]
    pub verbose: bool,
}

impl Command {
    fn print_base_info(&self){
        println!("corpus      : {}", self.corpus.display());
        println!("label       : {}", self.label.as_deref().unwrap_or("all"));
        println!("target      : {}", self.target);
        if let Some(ref cmp) = self.compare {
            println!("compare     : {cmp}");
        }
        println!("concurrency : {}", self.concurrency);
        println!("requests    : {}", self.requests.map(|v| v.to_string()).unwrap_or_else(|| "auto(corpus size)".to_string()));
        println!("shuffle-seed: {}", self.seed.map(|v| v.to_string()).unwrap_or_else(|| "random".to_string()));
        println!("output-dir  : {}", self.output_dir.display());
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
        println!("effective requests : {}", effective_requests);

        let runner = BenchRunner::new(&self.target, self.compare.as_deref(), self.concurrency)?;
        runner.run(corpus, self.requests, self.seed).await?;
        Ok(())
    }
}
