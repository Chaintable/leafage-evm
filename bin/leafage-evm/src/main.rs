mod metrics;
mod migrate;
mod runner;
mod standalone;
mod updater;

use clap::Parser;
use console_subscriber::ConsoleLayer;
use runner::Cli;
use std::time::Duration;
use tikv_jemallocator;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let layer = ConsoleLayer::builder()
        .retention(Duration::from_secs(1800))
        .with_default_env()
        .spawn();
    tracing_subscriber::registry()
        .with(layer)
        .with(fmt::layer().pretty())
        .with(EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    cli.run().await?;
    Ok(())
}
