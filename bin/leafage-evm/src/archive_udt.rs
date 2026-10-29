//! `leafage-evm archive-udt`: offline tools for archive schema v3, account
//! and storage versions keyed by RocksDB user-defined timestamps
//! (`docs/ArchiveSchemaV3.md`).

use anyhow::Result;
use leafage_evm_storage::{verify_udt_against_v2, ArchiveRocksDBStorage};
use std::path::PathBuf;
use tracing::info;

#[derive(Debug, clap::Parser)]
pub struct Command {
    #[clap(subcommand)]
    action: Action,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    /// Rewrite a v2 (inverted) archive as schema v3 into a new directory.
    /// The source is opened read-only.
    Convert {
        /// The v2 archive database.
        #[arg(long, value_name = "PATH")]
        src: PathBuf,
        /// The v3 archive to create; must not exist or be empty.
        #[arg(long, value_name = "PATH")]
        dst: PathBuf,
        /// Parallel workers (each versioned CF is split into 4 × jobs shards).
        #[arg(long, default_value = "8")]
        jobs: usize,
        /// Block cache of the destination while writing, in MiB.
        #[arg(long, default_value = "1024")]
        cache_mb: usize,
    },
    /// Compare a v3 archive with the v2 archive it was converted from.
    Verify {
        #[arg(long, value_name = "PATH")]
        v2: PathBuf,
        #[arg(long, value_name = "PATH")]
        v3: PathBuf,
        /// Random keys per versioned CF.
        #[arg(long, default_value = "20000")]
        samples: u64,
        #[arg(long, default_value = "1")]
        seed: u64,
    },
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        match &self.action {
            Action::Convert {
                src,
                dst,
                jobs,
                cache_mb,
            } => {
                let report =
                    ArchiveRocksDBStorage::convert_inverted_to_udt(src, dst, *cache_mb, *jobs)?;
                info!(target: "migrate", "archive-udt convert: {report:?}");
            }
            Action::Verify {
                v2,
                v3,
                samples,
                seed,
            } => {
                let report = verify_udt_against_v2(v2, v3, *samples, *seed)?;
                info!(target: "migrate", "archive-udt verify: {report:?}");
                if report.mismatches > 0 {
                    anyhow::bail!("{} mismatches", report.mismatches);
                }
            }
        }
        Ok(())
    }
}
