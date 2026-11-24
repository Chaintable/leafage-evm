//! `leafage-evm archive-udt`: offline tools for archive schema v3, account
//! and storage versions keyed by RocksDB user-defined timestamps
//! (`docs/ArchiveSchemaV3.md`).

use anyhow::{bail, Result};
use leafage_evm_storage::{
    init_archive_schema, set_inverted_block_encoding, verify_udt_against_v2, ArchiveRocksDBStorage,
    ArchiveSchema,
};
use std::path::{Path, PathBuf};
use tracing::info;

/// Before a RocksDB archive is opened: lay out a new one at `path` for
/// `schema` and fix the process-wide block encoding to it. An existing
/// archive keeps its own schema, which the open detects. Returns whether the
/// archive is new.
pub(crate) fn prepare_archive_schema(
    path: &Path,
    schema: ArchiveSchema,
    inverted_flag: bool,
) -> Result<bool> {
    let new = init_archive_schema(path, schema)?;
    if new {
        if schema == ArchiveSchema::V1 && inverted_flag {
            bail!("--archive-schema v1 conflicts with --inverted-block-encoding");
        }
        set_inverted_block_encoding(schema.inverted());
        info!("creating a new archive at {path:?} with schema {schema}");
    } else {
        info!("opening the existing archive at {path:?} with its own schema (--archive-schema {schema} only applies to new archives)");
    }
    Ok(new)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_v1_archive_conflicts_with_inverted_flag() {
        let dir = std::env::temp_dir().join(format!("leafage-schema-flag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let err = prepare_archive_schema(&dir, ArchiveSchema::V1, true).unwrap_err();
        assert!(err.to_string().contains("conflicts"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!("v3".parse::<ArchiveSchema>().unwrap(), ArchiveSchema::V3);
        assert_eq!("v1".parse::<ArchiveSchema>().unwrap(), ArchiveSchema::V1);
        assert!("3".parse::<ArchiveSchema>().is_err());
    }
}
