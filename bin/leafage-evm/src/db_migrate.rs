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
