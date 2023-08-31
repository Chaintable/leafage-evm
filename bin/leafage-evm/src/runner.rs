use crate::{migrate, standalone};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::future::Future;
use tokio::signal;
use tracing::info;

#[derive(Debug, Parser)]
#[command(author, version = "0.1.0",  about = "leafage-evm", long_about = None)]
pub(crate) struct Cli {
    /// The command to run
    #[clap(subcommand)]
    command: Commands,
}

impl Cli {
    pub(crate) async fn run(self) -> Result<()> {
        self.command.run().await
    }
}

/// Commands to be executed
#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Start the node
    #[command(name = "standalone")]
    Standalone(standalone::Command),
    #[command(name = "migrate")]
    Migrate(migrate::Command),
}

impl Commands {
    pub(crate) async fn run(self) -> Result<()> {
        match self {
            Commands::Standalone(mut cmd) => cmd.run().await,
            Commands::Migrate(mut cmd) => cmd.run().await,
        }
    }
}

/// Runs the future to completion or until:
/// - `ctrl-c` is received.
/// - `SIGTERM` is received.
pub(crate) async fn run_until_ctrl_c<F>(fut: F) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    let ctrl_c = signal::ctrl_c();
    let mut stream = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    let sigterm = stream.recv();

    tokio::select! {
        _ = ctrl_c => {
            info!(target: "leafage-evm::cli",  "Received ctrl-c");
            fut.await?;
        },
        _ = sigterm => {
            info!(target: "leafage-evm::cli",  "Received SIGTERM");
            fut.await?;
        },
    }
    Ok(())
}
