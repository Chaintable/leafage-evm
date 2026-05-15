use anyhow::Result;
use clap::Parser;
use leafage_evm_storage::ArchiveRocksDBStorage;
use std::path::PathBuf;
use tracing::info;

/// `leafage-evm compact` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to the database
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// Database cache size in MB
    #[arg(long, default_value = "2048")]
    cache_size: usize,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        info!(target: "compact", "Starting database compaction");
        info!(target: "compact", "db_path: {:?}, cache_size: {}MB", self.db_path, self.cache_size);

        // Open archive database with auto-compactions disabled: this command
        // drives its own range-segmented compact() (low memory by design), and
        // letting RocksDB also fire uncontrolled background compaction on the
        // bulk-load L0 backlog is what blows past the memory limit.
        let db = ArchiveRocksDBStorage::open(&self.db_path, self.cache_size, true);

        info!(target: "compact", "Database opened, starting compaction...");
        db.compact()?;
        info!(target: "compact", "Database compaction completed.");

        Ok(())
    }
}
