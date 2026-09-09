use crate::utils::{parse_kafka_s3_config, s3_get_block_info_by_number, KafkaS3Config};
use anyhow::{anyhow, bail, Result};
use clap::Parser;
use jsonrpsee::http_client::HttpClientBuilder;
use leafage_evm_storage::{
    set_inverted_block_encoding, EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper,
    StorageKind,
};
use leafage_evm_types::{BlockId, BlockNumberOrTag, BlockStorageDiff};
use std::path::PathBuf;
use tracing::info;

/// `leafage-evm rewind` command
///
/// Offline rewind. Stop the node before running this command.
///
/// Snapshot mode: the target block is resolved via --kafka-s3-config or
/// --rpc-addr (one is required), and until the replay catches up the
/// "latest" state is a mixture of old and replayed values — keep the node
/// out of serving rotation until it has switched to the Kafka tail.
///
/// Archive mode (--archive): delete account/storage versions and block indexes
/// above the target, then publish its head. Interrupted rewinds block normal
/// startup; rerun the same command to finish. Content-addressed code is retained.
/// Snapshot mode only resets the head; it does not restore historical state.
#[derive(Debug, Parser)]
pub struct Command {
    /// The path to the database to rewind.
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// The type of database.
    /// Default: rocksdb
    #[arg(long, default_value = "rocksdb")]
    db_type: StorageKind,

    /// The size of the database cache in MB.
    /// Default: 2048
    #[arg(long, default_value = "2048")]
    db_cache: usize,

    /// Whether the database was written in archive mode.
    /// Default: false
    ///
    /// Must match how the database was written: snapshot and archive share
    /// column family names but use different encodings.
    #[arg(long, default_value_t = false)]
    archive: bool,

    /// Use descending version keys for an archive without an encoding marker.
    /// A RocksDB encoding marker takes precedence over this fallback.
    #[arg(long, default_value_t = false)]
    inverted_block_encoding: bool,

    /// The block number to rewind the committed head to.
    #[arg(long)]
    to_block: u64,

    /// The kafka s3 config (absolute file path or inline JSON), used to
    /// resolve the target block info from S3 and locate the offset file.
    /// Required in snapshot mode unless --rpc-addr is given; optional in
    /// archive mode (only its offset_dir is used, if set).
    #[arg(long, value_parser = parse_kafka_s3_config, value_name = "KAFKA_S3_CONFIG_PATH")]
    kafka_s3_config: Option<KafkaS3Config>,

    /// Optional RPC endpoint for resolving the target block info instead of
    /// the S3 outer-bucket number index.
    #[arg(long, value_name = "URL")]
    rpc_addr: Option<String>,

    /// Keep the kafka offset file (snapshot mode only).
    /// Default: false
    ///
    /// By default the offset file is deleted so the next start falls back to
    /// the S3 catch-up path. A retained offset would resume Kafka at a
    /// position whose parent blocks no longer match the rewound head, making
    /// every update fail with ParentBlockHashNotFound.
    #[arg(long, default_value_t = false)]
    keep_offset: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(path: &std::path::Path) -> Command {
        Command::try_parse_from([
            "rewind",
            "--db-path",
            path.to_str().unwrap(),
            "--to-block",
            "1",
            "--archive",
        ])
        .unwrap()
    }

    #[test]
    fn archive_rewind_clears_default_and_custom_offsets() {
        let dir = std::env::temp_dir().join(format!("rewind-offset-{}", std::process::id()));
        let default_offset = dir.join("offset");
        let custom_offset = dir.join("custom");
        std::fs::create_dir_all(&default_offset).unwrap();
        std::fs::create_dir_all(&custom_offset).unwrap();
        std::fs::write(default_offset.join("offset"), "123").unwrap();
        std::fs::write(custom_offset.join("offset"), "456").unwrap();
        let mut cmd = command(&dir);
        cmd.clear_offset().unwrap();
        assert!(!default_offset.join("offset").exists());
        assert!(custom_offset.join("offset").exists());
        cmd.kafka_s3_config = Some(KafkaS3Config {
            offset_dir: custom_offset.to_str().unwrap().into(),
            ..Default::default()
        });
        cmd.clear_offset().unwrap();
        assert!(!custom_offset.join("offset").exists());
        cmd.clear_offset().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        command(&dir).clear_offset().unwrap();
    }

    #[tokio::test]
    async fn archive_rewind_rejects_keep_offset_before_opening_database() {
        let mut cmd = command(std::path::Path::new("/nonexistent-archive-rewind-test"));
        cmd.keep_offset = true;
        assert!(cmd
            .run()
            .await
            .unwrap_err()
            .to_string()
            .contains("--keep-offset"));
    }
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        if self.archive {
            if self.keep_offset {
                bail!("--keep-offset is incompatible with archive rewind; the old Kafka position belongs to the discarded history");
            }
            set_inverted_block_encoding(self.inverted_block_encoding);
            let db =
                MultiStorage::open_for_archive_rewind(&self.db_path, self.db_cache, self.db_type)?;
            let target = db.archive_rewind_target(self.to_block)?;
            info!(target: "rewind", number = self.to_block, hash = %target.header.hash, "truncating archive to target");
            // Do this before any deletion. A crash before marking the rewind
            // only causes a harmless catch-up from the old committed head.
            self.clear_offset()?;
            db.rewind_archive(self.to_block)?;
            info!(target: "rewind", number = self.to_block, "archive rewind complete; restart standalone to catch up");
            return Ok(());
        }
        let db = MultiStorage::open(
            self.db_path.as_path(),
            self.db_cache,
            self.db_type,
            self.archive,
            false,
            false,
        )?;
        let state = StateDBWrapper(
            db.db_at(BlockId::Number(BlockNumberOrTag::Latest))?
                .ok_or_else(|| anyhow!("no latest state in database"))?,
        );

        // Snapshot and archive DBs share CF names but encode block info
        // differently (JSON vs RLP), so a mismatched --archive flag opens
        // fine and only fails here.
        let current = state
            .last_committed_block()
            .map_err(|e| {
                anyhow!(e).context(
                    "failed to read the committed head; check that --archive matches \
                     how this database was written (snapshot and archive encodings differ)",
                )
            })?
            .ok_or_else(|| anyhow!("database is uninitialized, nothing to rewind"))?;
        info!(
            target: "rewind",
            "current committed head: number {}, hash {}",
            current.header.number, current.header.hash
        );
        if self.to_block >= current.header.number {
            bail!(
                "target block {} is not below the current committed head {}",
                self.to_block,
                current.header.number
            );
        }

        let target = {
            if self.kafka_s3_config.is_none() && self.rpc_addr.is_none() {
                bail!(
                    "snapshot rewind needs --kafka-s3-config or --rpc-addr \
                     to resolve the target block"
                );
            }
            let mut rpc_client = None;
            if let Some(rpc_url) = &self.rpc_addr {
                rpc_client = Some(HttpClientBuilder::default().build(rpc_url)?);
            }
            let s3_config = aws_config::load_from_env().await;
            let s3_client = aws_sdk_s3::Client::new(&s3_config);
            let cfg = self.kafka_s3_config.clone().unwrap_or_default();
            s3_get_block_info_by_number(
                &rpc_client,
                &s3_client,
                &cfg.bucket_name,
                &cfg.outer_bucket_name,
                &cfg.s3_chain_id,
                &cfg.version,
                self.to_block,
            )
            .await?
        };
        if target.header.number != self.to_block {
            bail!(
                "resolved block info has number {}, expected {}",
                target.header.number,
                self.to_block
            );
        }

        let target_hash = target.header.hash;
        // An empty diff makes update_block a pure pointer move: it re-inserts
        // the target's BlockInfo (snapshot mode prunes all but the newest)
        // and sets LatestBlockHash, without touching account/storage state.
        state.update_block(target, BlockStorageDiff::default())?;
        info!(
            target: "rewind",
            "rewound committed head to number {}, hash {}",
            self.to_block, target_hash
        );

        if !self.keep_offset {
            self.clear_offset()?;
        }

        info!(
            target: "rewind",
            "done; next standalone start will replay blocks {}..head from s3",
            self.to_block + 1
        );
        Ok(())
    }

    fn clear_offset(&self) -> Result<()> {
        let offset_dir = match &self.kafka_s3_config {
            Some(cfg) if !cfg.offset_dir.is_empty() => cfg.offset_dir.clone(),
            _ => format!("{}/offset", self.db_path.to_str().unwrap_or_default()),
        };
        let offset_file = format!("{}/offset", offset_dir);
        match std::fs::remove_file(&offset_file) {
            Ok(()) => info!(target: "rewind", "removed offset file {}", offset_file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!(target: "rewind", "no offset file at {}", offset_file)
            }
            Err(e) => return Err(e.into()),
        }
        // Archive deletion must not become durable ahead of offset removal.
        if self.archive {
            match std::fs::File::open(&offset_dir) {
                Ok(dir) => dir.sync_all()?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}
