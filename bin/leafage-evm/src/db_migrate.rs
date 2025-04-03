use anyhow::{Ok, Result};
use clap::Parser;
use leafage_evm_storage::{read_offset, write_offset, DBource};
use std::path::PathBuf;
/// `leafage-evm migrate` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to the dir which contains the archive database
    ///
    #[arg(long, value_name = "PATH")]
    archive: PathBuf,

    /// The path to the dir which state database generated
    ///
    #[arg(long, value_name = "PATH")]
    state: PathBuf,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        let db_source = DBource::new(&self.archive, &self.state);
        let offset =
            read_offset(&format!("{}/offset", self.archive.to_str().unwrap())).unwrap_or_default();
        if offset != 0 {
            write_offset(&format!("{}/offset", self.state.to_str().unwrap()), offset)?;
        }
        db_source.migrate()?;
        Ok(())
    }
}
