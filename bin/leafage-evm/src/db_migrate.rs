use anyhow::{bail, Ok, Result};
use clap::Parser;
use leafage_evm_storage::{
    read_offset, write_offset, ArchiveRocksDBStorage, DBSource, StorageKind,
};
use std::path::PathBuf;
/// `leafage-evm migrate` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to the dir which contains the source database
    ///
    #[arg(long, value_name = "PATH")]
    src: PathBuf,

    /// The source storage kind (rocksdb or mdbx)
    ///
    #[arg(long, value_name = "KIND", default_value = "rocksdb")]
    src_kind: StorageKind,

    /// Whether the source database is archive mode
    ///
    #[arg(long, default_value = "false")]
    src_is_archive: bool,

    /// Whether the source archive database uses the inverted (descending)
    /// block-height key encoding. Default: false (legacy ascending).
    ///
    /// Must match how the source archive was written (the `archive-init` /
    /// `standalone --inverted-block-encoding` flag). Only relevant when
    /// `--src-is-archive` is set; a mismatch makes the latest-state scan read
    /// the wrong version per key and silently produce a corrupt snapshot.
    #[arg(long, default_value = "false")]
    inverted_block_encoding: bool,

    /// Wire/disk codec for state diffs and account values (`standard` or
    /// `blast-v1`). Must match the database and the S3 feed; a record of the
    /// other shape is a read error. Default: standard.
    #[arg(long, value_parser = crate::utils::parse_state_diff_codec, default_value = "standard")]
    state_diff_codec: leafage_evm_types::StateDiffCodec,

    /// The path to the dir which state database generated
    ///
    #[arg(long, value_name = "PATH")]
    dst: PathBuf,

    /// The destination storage kind (rocksdb or mdbx)
    ///
    #[arg(long, value_name = "KIND", default_value = "rocksdb")]
    dst_kind: StorageKind,

    /// Cache size in MB
    ///
    #[arg(long, default_value = "1024")]
    cache_size: usize,

    /// Rebuild a **legacy (ascending)** archive DB into the **inverted
    /// (descending)** block-height key encoding in place of the normal
    /// archive→state migration.
    ///
    /// This is an archive→archive re-encode: it rewrites only the
    /// account/storage version tails (block_num → u64::MAX - block_num), copies
    /// every other column family verbatim, and drops orphaned pre-#104
    /// dual-write `u64::MAX` latest-pointer rows. The source is opened
    /// read-only and never modified; `--dst` is created fresh.
    ///
    /// Requires `--src-is-archive`, rocksdb for both kinds, and a legacy source
    /// (`--inverted-block-encoding=false`). The resulting DB is what a node run
    /// with `--inverted-block-encoding` expects, with no S3 re-sync.
    #[arg(long, default_value = "false")]
    reencode_inverted: bool,

    /// Parallel worker threads for `--reencode-inverted` (sharded by leading
    /// key byte). 0 = auto (one per core, capped at 16). Higher values speed up
    /// the CPU/merge-bound scan on multi-core hosts; lower values reduce disk
    /// contention on slow storage.
    #[arg(long, default_value = "0")]
    reencode_jobs: usize,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        // Fix the account value codec before any account decode/encode; must
        // match the source database's account format.
        leafage_evm_storage::set_account_codec(self.state_diff_codec);

        // Archive→archive re-encode mode: a raw byte-level rebuild that does not
        // go through the latest-state iterators (so it preserves every
        // historical version, not just the tip).
        if self.reencode_inverted {
            if !self.src_is_archive {
                bail!("--reencode-inverted requires --src-is-archive (source must be a legacy archive DB)");
            }
            if self.inverted_block_encoding {
                bail!("--reencode-inverted converts legacy→inverted; the source must be legacy (drop --inverted-block-encoding)");
            }
            if !matches!(self.src_kind, StorageKind::Rocksdb)
                || !matches!(self.dst_kind, StorageKind::Rocksdb)
            {
                bail!("--reencode-inverted only supports rocksdb archive DBs");
            }
            ArchiveRocksDBStorage::reencode_legacy_to_inverted(
                &self.src,
                &self.dst,
                self.cache_size,
                self.reencode_jobs,
            )?;
            // Carry the resync offset over, same as the normal migration.
            let offset =
                read_offset(&format!("{}/offset", self.src.to_str().unwrap())).unwrap_or_default();
            if offset != 0 {
                write_offset(&format!("{}/offset", self.dst.to_str().unwrap()), offset)?;
            }
            return Ok(());
        }

        // Fix the versioned-key encoding before reading the archive source via
        // the latest-state iterators (the only place db-migrate decodes
        // archive keys).
        leafage_evm_storage::set_inverted_block_encoding(self.inverted_block_encoding);
        let db_source = DBSource::new(
            &self.src,
            self.src_kind,
            self.src_is_archive,
            &self.dst,
            self.dst_kind,
            self.cache_size,
        )?;
        let offset =
            read_offset(&format!("{}/offset", self.src.to_str().unwrap())).unwrap_or_default();
        if offset != 0 {
            write_offset(&format!("{}/offset", self.dst.to_str().unwrap()), offset)?;
        }
        db_source.migrate()?;
        Ok(())
    }
}
