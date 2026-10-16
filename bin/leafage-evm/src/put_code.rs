use alloy::primitives::keccak256;
use anyhow::{bail, Context, Result};
use leafage_evm_storage::{ArchiveRocksDBStorage, StateDBWrite};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

/// `leafage-evm put-code` command.
///
/// Writes contract code into the `HashToCode` column family of an archive
/// database, keyed by its keccak hash. This repairs a DB whose accounts point
/// at a code hash it has no code for (it serves `0x` for them), e.g. the BSC
/// system contract upgrades between MirrorSync and Feynman.
///
/// Run it **offline**: stop the serving node first, since the DB can only be
/// opened by one process. Codes already present are left untouched, so it is
/// safe to re-run.
#[derive(Debug, clap::Parser)]
pub struct Command {
    /// The path to the archive database.
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// Files holding one contract code each, as hex (with or without `0x`).
    #[arg(long = "code-file", value_name = "PATH", required = true)]
    code_files: Vec<PathBuf>,

    /// Database cache size in MB.
    #[arg(long, default_value = "1024")]
    cache_size: usize,

    /// Must match how the DB is served (see `standalone`).
    #[arg(long, default_value = "false")]
    archive_zstd_compression: bool,

    /// Only report which codes are missing.
    #[arg(long, default_value = "false")]
    dry_run: bool,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        let mut codes = Vec::with_capacity(self.code_files.len());
        for path in &self.code_files {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let code = alloy::hex::decode(text.trim())
                .with_context(|| format!("{} is not hex", path.display()))?;
            if code.is_empty() {
                bail!("{} holds no code", path.display());
            }
            codes.push((path, keccak256(&code), code));
        }

        // A typo'd path would otherwise create an empty DB.
        if !self.db_path.join("CURRENT").exists() {
            bail!("no RocksDB database found at {:?}", self.db_path);
        }
        let db = Arc::new(ArchiveRocksDBStorage::open(
            &self.db_path,
            self.cache_size,
            true,
            self.archive_zstd_compression,
        ));

        let mut batch = db.prepare_write_batch()?;
        let mut missing = 0;
        for (path, hash, code) in &codes {
            match db.read_code(*hash)? {
                Some(stored) if stored.as_ref() == code.as_slice() => {
                    info!(target: "put_code", "{hash} already present ({})", path.display());
                }
                Some(_) => bail!("{hash} is stored with different code"),
                None => {
                    info!(
                        target: "put_code",
                        "{hash} missing, {} bytes from {}",
                        code.len(),
                        path.display()
                    );
                    missing += 1;
                    db.write_code(&mut batch, hash.0.into(), code.clone().into())?;
                }
            }
        }
        if self.dry_run || missing == 0 {
            info!(target: "put_code", "{missing} of {} codes missing, nothing written", codes.len());
            return Ok(());
        }
        db.commit(batch)?;
        for (_, hash, code) in &codes {
            match db.read_code(*hash)? {
                Some(stored) if stored.as_ref() == code.as_slice() => {}
                _ => bail!("{hash} did not read back after the write"),
            }
        }
        info!(target: "put_code", "wrote {missing} of {} codes and read them back", codes.len());
        Ok(())
    }
}
