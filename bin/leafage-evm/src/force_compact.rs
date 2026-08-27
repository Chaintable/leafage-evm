use anyhow::Result;
use leafage_evm_storage::ArchiveRocksDBStorage;
use std::path::PathBuf;
use std::time::Instant;
use tracing::{info, warn};

/// `leafage-evm force-compact` command.
///
/// Full, forced (bottommost-level) compaction of an existing archive database —
/// the **repair** entry point for production storage that was bulk-loaded before
/// the compaction fix.
///
/// # Why this exists
///
/// `archive-init` and `db-migrate --reencode-inverted` build the versioned CFs by
/// writing external SST files with `SstFileWriter`. Those files carry only
/// compression — no prefix extractor, no bloom filter, non-partitioned index.
/// On ingest into a fresh DB, RocksDB places most of them directly at the
/// bottommost level, and a normal manual compaction leaves bottommost files
/// untouched — so they stay filter-less. That silently defeats the
/// inverted-encoding read path (forward `Seek` can no longer use the per-CF
/// prefix bloom to skip SSTs), which shows up as slow `eth_call` / `getStorageAt`
/// and an archive node whose OS page cache never "warms up".
///
/// This command reopens the DB and runs a forced-bottommost full compaction,
/// rewriting every level through the CF's block-based table factory. That
/// regenerates the prefix bloom + partitioned index the read path is tuned for
/// (and resizes the giant ingested SSTs down to `target_file_size_base`).
///
/// # Operational notes
///
/// * Run this **offline**: RocksDB is single-writer, so the serving node must be
///   stopped first (the DB path can only be opened by one process). Typical flow:
///   scale the pod down → run `force-compact` against the same volume → scale up.
/// * Cost is roughly **one full rewrite of the DB** (O(DB size) of read+write
///   I/O and temporary disk). It runs range-segmented (16 sub-ranges per large
///   CF) to bound peak memory, but it is not cheap — budget accordingly.
/// * Idempotent: safe to re-run. A DB that is already fully compacted with
///   filters present will still be rewritten (forced), so only run when needed.
#[derive(Debug, clap::Parser)]
pub struct Command {
    /// The path to the archive database to repair.
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// Database cache size in MB.
    #[arg(long, default_value = "2048")]
    cache_size: usize,

    /// Use ZSTD-with-dict compression at deep levels for the three large archive
    /// CFs. Must match how the DB was built / is served (see the `standalone`
    /// `--archive-zstd-compression` flag). Default: false (uniform LZ4).
    #[arg(long, default_value = "false")]
    archive_zstd_compression: bool,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        warn!(
            target: "force_compact",
            "force-compact rewrites the ENTIRE bottommost level (~one full DB \
             rewrite of I/O + temporary disk) to regenerate the prefix bloom / \
             partitioned index. Ensure the serving node is stopped and enough \
             free disk is available before proceeding."
        );
        info!(
            target: "force_compact",
            "db_path: {:?}, cache_size: {}MB, archive_zstd_compression: {}",
            self.db_path, self.cache_size, self.archive_zstd_compression,
        );

        // Fail fast on a wrong path. The archive DB is opened with
        // `create_if_missing` / `create_missing_column_families` = true, so a
        // typo'd path would otherwise create an empty DB and "repair" it
        // successfully. A real RocksDB directory always has a `CURRENT` file.
        let current = self.db_path.join("CURRENT");
        if !current.exists() {
            anyhow::bail!(
                "no RocksDB database found at {:?} (missing CURRENT file). Refusing to run: \
                 pointing force-compact at a non-existent path would create an empty DB and \
                 report a bogus success. Double-check --db-path.",
                self.db_path
            );
        }

        // Open with auto-compactions disabled: this command drives its own
        // range-segmented, forced compaction (low memory by design), and letting
        // RocksDB also fire uncontrolled background compaction would compete for
        // disk bandwidth and blow past the memory budget.
        let db = ArchiveRocksDBStorage::open(
            &self.db_path,
            self.cache_size,
            true,
            self.archive_zstd_compression,
        );

        info!(target: "force_compact", "Database opened, starting forced full compaction...");
        let start = Instant::now();
        // `compact()` forces bottommost-level compaction, which is exactly the
        // rewrite that regenerates the missing prefix bloom + partitioned index.
        db.compact()?;

        // rust-rocksdb's compact API discards the CompactRange Status, so a
        // silently-failed compaction would otherwise look successful. Verify the
        // outcome: if any oversized (un-rewritten, filter-less) SST remains in
        // the versioned CFs, this returns an error instead of a bogus success.
        db.verify_archive_compacted()?;

        info!(
            target: "force_compact",
            "Forced full compaction completed and verified in {:.1}s. Prefix bloom + \
             partitioned index regenerated across all column families.",
            start.elapsed().as_secs_f64(),
        );

        Ok(())
    }
}
