//! `leafage-evm archive-v4`: offline tools for archive schema v4, account
//! and storage versions in page files outside RocksDB
//! (`docs/ArchiveSchemaV4.md`).

use anyhow::{bail, Result};
use leafage_evm_storage::{
    replay_get_trace, set_inverted_block_encoding, verify_v4_against_v3, ArchiveRocksDBStorage,
};
use std::path::PathBuf;
use tracing::info;

#[derive(Debug, clap::Parser)]
pub struct Command {
    #[clap(subcommand)]
    action: Action,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    /// Rewrite a v3 archive as schema v4 into a new directory. The source
    /// is opened read-only. `ARCHIVE_V4_BASE_ZSTD_LEVEL` (default 3) and
    /// `ARCHIVE_V4_BASE_FILE_MB` (default 8192) shape the files written.
    Convert {
        /// The v3 archive database.
        #[arg(long, value_name = "PATH")]
        src: PathBuf,
        /// The v4 archive to create; must not exist or be empty.
        #[arg(long, value_name = "PATH")]
        dst: PathBuf,
        /// Parallel workers (each versioned CF is split into 4 × jobs shards).
        #[arg(long, default_value = "8")]
        jobs: usize,
    },
    /// Compare a v4 archive with the v3 archive it was converted from.
    Verify {
        #[arg(long, value_name = "PATH")]
        v3: PathBuf,
        #[arg(long, value_name = "PATH")]
        v4: PathBuf,
        /// Random keys per kind (accounts, storage).
        #[arg(long, default_value = "20000")]
        samples: u64,
        #[arg(long, default_value = "1")]
        seed: u64,
    },
    /// Fold the whole delta of a v4 archive into its base files, unthrottled.
    /// The node does this by itself in the background; this is for an
    /// archive that is not serving (after a bulk load, before a snapshot).
    Compact {
        #[arg(long, value_name = "PATH")]
        db_path: PathBuf,
        /// Memory for the store's index and page cache, in MiB.
        #[arg(long, default_value = "2048")]
        cache_mb: usize,
    },
    /// Benchmark: replay a captured trace of archive lookups against the
    /// files of a v4 archive and check the version each one finds. A record
    /// is `cf u8 (4|5) | key length u8 | key | height u64 LE | found block
    /// u64 LE`.
    ReplayGets {
        #[arg(long, value_name = "PATH")]
        db_path: PathBuf,
        #[arg(long, value_name = "PATH")]
        trace: PathBuf,
        #[arg(long, default_value = "16")]
        threads: usize,
        /// Memory for the store's index, filters and page cache, in MiB.
        #[arg(long, default_value = "8192")]
        cache_mb: u64,
        /// Lookups that fill the cache before the measured part.
        #[arg(long, default_value = "0")]
        warmup: u64,
    },
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        match &self.action {
            Action::Convert { src, dst, jobs } => {
                let report = ArchiveRocksDBStorage::convert_udt_to_v4(src, dst, *jobs)?;
                info!(target: "migrate", "archive-v4 convert: {report:?}");
            }
            Action::Verify {
                v3,
                v4,
                samples,
                seed,
            } => {
                let report = verify_v4_against_v3(v3, v4, *samples, *seed)?;
                info!(target: "migrate", "archive-v4 verify: {report:?}");
                if report.mismatches > 0 {
                    bail!("{} mismatches", report.mismatches);
                }
            }
            Action::ReplayGets {
                db_path,
                trace,
                threads,
                cache_mb,
                warmup,
            } => {
                let [warm, run] = replay_get_trace(db_path, trace, *threads, *cache_mb, *warmup)?;
                for (name, r) in [("warm-up", warm), ("measured", run)] {
                    let n = r.gets.max(1) as f64;
                    println!(
                        "{name}: {} gets in {:.1}s -> {:.0} gets/s | device reads/get {:.4}, {:.2} KiB/get, {:.2} ms/read | \
                         CPU {:.2} us/get ({:.2} cores) | latency us p50 {} p90 {} p99 {} p99.9 {} | mismatches {}",
                        r.gets,
                        r.seconds,
                        n / r.seconds.max(1e-9),
                        r.reads as f64 / n,
                        r.read_bytes as f64 / n / 1024.0,
                        r.read_seconds * 1e3 / r.reads.max(1) as f64,
                        r.cpu_seconds * 1e6 / n,
                        r.cpu_seconds / r.seconds.max(1e-9),
                        r.latency_us[0],
                        r.latency_us[1],
                        r.latency_us[2],
                        r.latency_us[3],
                        r.mismatches,
                    );
                }
            }
            Action::Compact { db_path, cache_mb } => {
                // A v4 archive carries its encoding marker; the open aligns
                // the process to it.
                set_inverted_block_encoding(true);
                let db = ArchiveRocksDBStorage::open_for_bulk_load(db_path, *cache_mb, false);
                if !db.is_schema_v4() {
                    bail!("{} is not a schema v4 archive", db_path.display());
                }
                db.v4_compact()?;
                let (files, base, segments, delta, index, filters) =
                    db.v4_sizes().expect("a v4 archive");
                info!(
                    target: "migrate",
                    "archive-v4 compact: {files} base files of {base} bytes, {segments} segments of {delta} bytes, index {index} bytes, filters {filters} bytes"
                );
            }
        }
        Ok(())
    }
}
