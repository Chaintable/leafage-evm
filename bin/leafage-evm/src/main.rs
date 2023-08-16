mod migrate;
mod runner;
mod standalone;
mod updater;

use clap::Parser;
use runner::Cli;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    cli.run().await?;
    Ok(())
}
