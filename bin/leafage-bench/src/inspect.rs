use std::path::PathBuf;

use anyhow::Result;
use clap::Args;

use crate::corpus::Corpus;

/// Inspect the corpus file: print summary statistics without running any benchmark.
#[derive(Debug, Args)]
pub struct Command {
    /// Path to the corpus JSON file.
    ///
    /// Can also be set via the LEAFAGE_BENCH_CORPUS environment variable.
    #[arg(long, short)]
    pub corpus: PathBuf,
}

impl Command {
    pub fn run(self) -> Result<()> {
        let corpus = Corpus::load(&self.corpus)?;

        println!("file        : {}", self.corpus.display());
        println!("generated   : {}", corpus.meta.generated_at);
        println!("format      : {}", corpus.meta.format);
        println!("seed        : {}", corpus.meta.seed);
        println!("stage       : {}", corpus.meta.stage);
        println!("total cases : {}", corpus.meta.case_count);
        println!();
        println!("quotas:");
        for label in ["L1", "L2", "L3"] {
            let quota = corpus.meta.quotas.get(label).copied().unwrap_or(0);
            let actual = corpus
                .cases
                .iter()
                .filter(|c| c.classification.label.as_str() == label)
                .count();
            println!("  {label} : quota={quota}  actual={actual}");
        }
        println!();
        println!("ingest stats:");
        println!("  requests_received : {}", corpus.meta.ingest_stats.requests_received);
        println!("  cases_ingested    : {}", corpus.meta.ingest_stats.cases_ingested);
        println!("  rpc_objects_found : {}", corpus.meta.ingest_stats.rpc_objects_found);

        Ok(())
    }
}

