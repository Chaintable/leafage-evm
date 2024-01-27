mod metrics;
mod migrate;
mod runner;
mod standalone;
mod updater;

use clap::Parser;
use console_subscriber::ConsoleLayer;
use num_cpus;
use runner::Cli;
use std::time::Duration;
use tikv_jemallocator;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> anyhow::Result<()> {
    let mut core_num = num_cpus::get();
    if core_num <= 1 {
        core_num = 4;
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(core_num)
        .max_blocking_threads(1024)
        .build()
        .unwrap()
        .block_on(async {
            let layer = ConsoleLayer::builder()
                .retention(Duration::from_secs(1800))
                .with_default_env()
                .spawn();
            tracing_subscriber::registry()
                .with(layer)
                .with(fmt::layer().pretty())
                .with(EnvFilter::from_default_env())
                .init();
            info!("Starting leafage-evm, number cpu {}", core_num);
            let cli = Cli::parse();
            cli.run().await?;
            Ok(())
        })
}
