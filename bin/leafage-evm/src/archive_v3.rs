//! `leafage-evm archive-v3`: offline tools for archive schema v3 sealed
//! history (`docs/ArchiveSchemaV3.md`).

use anyhow::Result;
use leafage_evm_storage::{build_history_v3, verify_history_v3};
use std::path::PathBuf;
use tracing::info;

#[derive(Debug, clap::Parser)]
pub struct Command {
    #[clap(subcommand)]
    action: Action,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    /// Build epoch 0 (`[0, head]`) of the sealed history from a v2
    /// (inverted) archive. The archive is opened read-only.
    Build {
        /// The v2 archive database.
        #[arg(long, value_name = "PATH")]
        db_path: PathBuf,
        /// Output directory for the manifest and segments.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
        /// Parallel shard builders.
        #[arg(long, default_value = "8")]
        jobs: usize,
        /// Shards per CF = 2^shard_bits, by the top bits of the key.
        #[arg(long, default_value = "4")]
        shard_bits: u8,
        /// RocksDB iterator readahead in MiB.
        #[arg(long, default_value = "8")]
        readahead_mb: usize,
    },
    /// Compare sealed history with the v2 archive it was built from.
    Verify {
        #[arg(long, value_name = "PATH")]
        db_path: PathBuf,
        #[arg(long, value_name = "PATH")]
        history: PathBuf,
        /// Random keys per CF.
        #[arg(long, default_value = "20000")]
        samples: u64,
        #[arg(long, default_value = "1")]
        seed: u64,
    },
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        match &self.action {
            Action::Build {
                db_path,
                out,
                jobs,
                shard_bits,
                readahead_mb,
            } => {
                let manifest =
                    build_history_v3(db_path, out, *jobs, *shard_bits, readahead_mb << 20)?;
                info!(target: "history_v3", "manifest: sealed up to {}", manifest.sealed_upto);
            }
            Action::Verify {
                db_path,
                history,
                samples,
                seed,
            } => {
                let report = verify_history_v3(db_path, history, *samples, *seed)?;
                info!(target: "history_v3", "verify: {report:?}");
                if report.mismatches > 0 {
                    anyhow::bail!("{} mismatches", report.mismatches);
                }
            }
        }
        Ok(())
    }
}
