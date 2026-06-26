// cache-verify round 2: validating Plan C (layer-cache via cargo-chef, no cache mounts)
mod archive_init;
mod archive_scan;
mod compact;
mod db_migrate;
mod initializer;
mod register;
mod rewind;
mod runner;
mod snapshot_init;
mod standalone;
mod updater;
mod utils;
mod pprof;
mod warm;

use clap::Parser;
use runner::Cli;
use tikv_jemallocator;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> anyhow::Result<()> {
    let mut core_num = std::thread::available_parallelism()?.get();
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
            tracing_subscriber::registry()
                .with(fmt::layer().pretty())
                .with(EnvFilter::from_default_env())
                .init();
            info!("Starting leafage-evm, number cpu {}", core_num);
            let cli = Cli::parse();
            cli.run().await?;
            Ok(())
        })
}
