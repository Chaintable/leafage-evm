use crate::{archive_init, archive_scan, compact, db_migrate, force_compact, rewind, standalone};
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
    #[command(name = "db-migrate")]
    DBMigrate(db_migrate::Command),
    /// Initialize archive database from S3 and RPC
    #[command(name = "archive-init")]
    ArchiveInit(archive_init::Command),
    /// Compact database to optimize storage
    #[command(name = "compact")]
    Compact(compact::Command),
    /// Forced full compaction to repair an archive DB whose bulk-loaded SSTs
    /// are missing the prefix bloom / partitioned index (regenerates them)
    #[command(name = "force-compact")]
    ForceCompact(force_compact::Command),
    /// Rewind the committed head to an earlier block to resync from S3
    #[command(name = "rewind")]
    Rewind(rewind::Command),
    /// Read-only scan of an archive DB column family (forensics/debugging)
    #[command(name = "archive-scan")]
    ArchiveScan(archive_scan::Command),
}

impl Commands {
    pub(crate) async fn run(self) -> Result<()> {
        match self {
            Commands::Standalone(mut cmd) => cmd.run().await,
            Commands::DBMigrate(mut cmd) => cmd.run().await,
            Commands::ArchiveInit(mut cmd) => cmd.run().await,
            Commands::Compact(mut cmd) => cmd.run().await,
            Commands::ForceCompact(mut cmd) => cmd.run().await,
            Commands::Rewind(mut cmd) => cmd.run().await,
            Commands::ArchiveScan(mut cmd) => cmd.run().await,
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
