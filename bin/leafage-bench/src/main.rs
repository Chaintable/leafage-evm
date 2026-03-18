mod corpus;
mod inspect;
mod run;
mod bench_runner;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// leafage-bench — eth_call performance benchmark for leafage-evm vs geth.
#[derive(Debug, Parser)]
#[command(
    name = "leafage-bench",
    version,
    about = "Benchmark eth_call performance across EVM RPC endpoints",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the benchmark against one or two RPC endpoints.
    Run(run::Command),

    /// Inspect the corpus file: print summary statistics without running any benchmark.
    Inspect(inspect::Command),
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Run(cmd) => cmd.run().await,
        Command::Inspect(cmd) => cmd.run(),
    }
}
