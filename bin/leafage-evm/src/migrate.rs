use anyhow::Result;
use clap::Parser;
use leafage_evm_storage::GethReader;
use std::path::PathBuf;

/// `leafage-evm migrate` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to the database to use for geth snapshot
    ///
    /// current support leveldb
    #[arg(long, value_name = "PATH")]
    geth_db_path: PathBuf,

    /// The path to the database to use for leafage-evm
    ///
    /// current support rocksdb
    db_path: PathBuf,

    /// The type of database to use for this node.
    /// Default: rocksdb
    /// current support rocksdb and leveldb
    #[arg(long, default_value = "rocksdb")]
    db_type: String,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        let mut geth_db = GethReader::open(self.geth_db_path.as_path());
        let mut count = 0;
        let _ = geth_db.account_scan(
            |address, account| {
                println!("address: {:?}, account: {:?}", address, account);
                count += 1;
                count <= 10
            },
            |address, index, value| {
                println!(
                    "address: {:?}, index: {:?}, value: {:?}",
                    address, index, value
                );
                true
            },
        );
        Ok(())
    }
}
