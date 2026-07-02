use anyhow::Result;
use clap::Parser;
use leafage_evm_storage::ArchiveRocksDBStorage;
use std::path::PathBuf;
use tracing::info;

/// `leafage-evm compact` command.
///
/// Runs the archive DB's range-segmented `compact()`. As of the bottommost-force
/// fix this rewrites the bottommost level too, so it also regenerates the prefix
/// bloom / partitioned index on bulk-loaded SSTs — i.e. it is now functionally a
/// full rewrite, not a light "optimize" pass. For repairing an existing
/// production DB, prefer the `force-compact` command, which is the documented
/// repair entry point (path guard + post-compaction verification).
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to the database
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// Database cache size in MB
    #[arg(long, default_value = "2048")]
    cache_size: usize,

    /// Use ZSTD-with-dict compression at deep levels for the three large
    /// archive CFs. Default: false (uniform LZ4). See standalone command
    /// `--archive-zstd-compression` for full trade-off notes.
    #[arg(long, default_value = "false")]
    archive_zstd_compression: bool,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        info!(target: "compact", "Starting database compaction");
        info!(
            target: "compact",
            "db_path: {:?}, cache_size: {}MB, archive_zstd_compression: {}",
            self.db_path, self.cache_size, self.archive_zstd_compression,
        );

        // Open archive database with auto-compactions disabled: this command
        // drives its own range-segmented compact() (low memory by design), and
        // letting RocksDB also fire uncontrolled background compaction on the
        // bulk-load L0 backlog is what blows past the memory limit.
        let db = ArchiveRocksDBStorage::open(
            &self.db_path,
            self.cache_size,
            true,
            self.archive_zstd_compression,
        );

        info!(target: "compact", "Database opened, starting compaction...");
        db.compact()?;
        info!(target: "compact", "Database compaction completed.");

        Ok(())
    }
}
