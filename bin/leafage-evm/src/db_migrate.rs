use anyhow::{Ok, Result};
use clap::Parser;
use leafage_evm_storage::{read_offset, write_offset, DBSource, StorageKind};
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
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
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
