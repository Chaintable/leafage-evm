use crate::utils::{parse_kafka_s3_config, s3_get_block_info_by_number, KafkaS3Config};
use anyhow::{anyhow, bail, Result};
use clap::{Parser, ValueEnum};
use jsonrpsee::http_client::HttpClientBuilder;
use leafage_evm_storage::{
    ArchiveRewindOffset, EvmStorageWrite, MultiStorage, OffsetSource, StateDBProvider, StateDBRead,
    StateDBWrapper, StorageKind,
};
use leafage_evm_types::{BlockId, BlockNumberOrTag, BlockStorageDiff, H256};
use std::path::PathBuf;
use tracing::info;

/// `leafage-evm rewind` command
///
/// Rewind the database's committed-head pointer to an earlier block so the
/// next `standalone` start resyncs `to_block + 1 ..= head` from S3.
///
/// The state itself is left untouched: `BlockStorageDiff` carries absolute
/// post-state values, so the forward replay converges to the exact head
/// state.
///
/// Snapshot mode: the target block is resolved via --kafka-s3-config or
/// --rpc-addr (one is required), and until the replay catches up the
/// "latest" state is a mixture of old and replayed values — keep the node
/// out of serving rotation until it has switched to the Kafka tail.
///
/// Archive mode resolves the target locally and only moves the head by default.
/// Add --truncate-archive to delete future versions in place. Stop all database
/// users first; an interrupted truncation must be resumed with the same options.
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

    /// Delete future archive state and indexes in place (offline only).
    #[arg(long, requires = "archive", conflicts_with = "keep_offset")]
    truncate_archive: bool,

    /// Trusted key encoding, required for an archive without an encoding marker.
    #[arg(long, value_enum, requires = "truncate_archive")]
    archive_encoding: Option<ArchiveEncoding>,

    /// Directory containing the Kafka offset file used by this node.
    #[arg(long, requires = "truncate_archive", conflicts_with_all = ["kafka_s3_config", "no_kafka"])]
    offset_dir: Option<PathBuf>,

    /// Explicitly declare that this archive does not consume Kafka.
    #[arg(
        long,
        requires = "truncate_archive",
        conflicts_with = "kafka_s3_config"
    )]
    no_kafka: bool,

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

    /// Keep the kafka offset file.
    /// Default: false
    ///
    /// By default the offset file is deleted so the next start falls back to
    /// the S3 catch-up path. A retained offset would resume Kafka at a
    /// position whose parent blocks no longer match the rewound head, making
    /// every update fail with ParentBlockHashNotFound.
    #[arg(long, default_value_t = false)]
    keep_offset: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ArchiveEncoding {
    Legacy,
    Inverted,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        if self.truncate_archive {
            if !self.archive || self.keep_offset {
                bail!("--truncate-archive requires --archive and forbids --keep-offset");
            }
            let offset = self.truncate_offset()?;
            let db =
                MultiStorage::open_for_archive_rewind(&self.db_path, self.db_cache, self.db_type)?;
            let target = db.rewind_archive(
                self.to_block,
                self.archive_encoding
                    .map(|e| matches!(e, ArchiveEncoding::Inverted)),
                offset,
            )?;
            info!(target: "rewind", number = target.header.number, hash = %target.header.hash, "archive truncation complete");
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

        let target = if self.archive {
            // Archive keeps every block info and the full number->hash
            // index locally, so no S3/RPC lookup is needed.
            let target_hash = state.0.read_block_hash(self.to_block)?;
            if target_hash == H256::ZERO {
                bail!("block {} not found in the archive database", self.to_block);
            }
            state.0.read_block_info(target_hash)?.ok_or_else(|| {
                anyhow!("block info for {target_hash} not found in the archive database")
            })?
        } else {
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
        }

        info!(
            target: "rewind",
            "done; next standalone start will replay blocks {}..head from s3",
            self.to_block + 1
        );
        Ok(())
    }
    fn truncate_offset(&self) -> Result<ArchiveRewindOffset> {
        if usize::from(self.no_kafka)
            + usize::from(self.offset_dir.is_some())
            + usize::from(self.kafka_s3_config.is_some())
            != 1
        {
            bail!("--truncate-archive needs exactly one of --kafka-s3-config, --offset-dir, or --no-kafka");
        }
        if self.no_kafka {
            return Ok(ArchiveRewindOffset::NoKafka);
        }
        let (dir, source) = match &self.kafka_s3_config {
            Some(cfg) => (
                if cfg.offset_dir.is_empty() {
                    self.db_path.join("offset")
                } else {
                    PathBuf::from(&cfg.offset_dir)
                },
                OffsetSource::KafkaConfig,
            ),
            None => (self.offset_dir.clone().unwrap(), OffsetSource::Directory),
        };
        Ok(ArchiveRewindOffset::Kafka {
            file: leafage_evm_storage::archive_offset_file(&dir)?,
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<&'static str> {
        vec!["rewind", "--db-path", "/unused", "--to-block", "1"]
    }

    #[test]
    fn truncate_options_are_opt_in_and_validate_offset_source() {
        let mut a = args();
        a.extend(["--archive", "--keep-offset"]);
        let cmd = Command::try_parse_from(a).unwrap();
        assert!(!cmd.truncate_archive);
        let mut a = args();
        a.push("--truncate-archive");
        assert!(Command::try_parse_from(a).is_err());
        let mut a = args();
        a.extend(["--archive", "--truncate-archive", "--keep-offset"]);
        assert!(Command::try_parse_from(a).is_err());
        let mut a = args();
        a.extend(["--archive", "--truncate-archive"]);
        let cmd = Command::try_parse_from(a.clone()).unwrap();
        assert!(cmd.truncate_offset().is_err());
        a.extend(["--no-kafka", "--archive-encoding", "legacy"]);
        assert_eq!(
            Command::try_parse_from(a.clone())
                .unwrap()
                .truncate_offset()
                .unwrap(),
            ArchiveRewindOffset::NoKafka
        );
        a.extend(["--offset-dir", "/unused"]);
        assert!(Command::try_parse_from(a).is_err());
    }

    #[test]
    fn archive_rewind_resolves_default_and_custom_offsets() {
        let mut a = args();
        a.extend(["--archive", "--truncate-archive"]);
        let mut cmd = Command::try_parse_from(a).unwrap();
        cmd.db_path = std::env::temp_dir();
        cmd.kafka_s3_config = Some(KafkaS3Config::default());
        assert_eq!(
            cmd.truncate_offset().unwrap(),
            ArchiveRewindOffset::Kafka {
                file: leafage_evm_storage::archive_offset_file(&cmd.db_path.join("offset"))
                    .unwrap(),
                source: OffsetSource::KafkaConfig,
            }
        );
        cmd.kafka_s3_config.as_mut().unwrap().offset_dir =
            std::env::temp_dir().to_str().unwrap().into();
        let custom = cmd.truncate_offset().unwrap();
        cmd.kafka_s3_config = None;
        cmd.offset_dir = Some(std::env::temp_dir());
        let ArchiveRewindOffset::Kafka { file, .. } = custom else {
            panic!()
        };
        assert_eq!(
            cmd.truncate_offset().unwrap(),
            ArchiveRewindOffset::Kafka {
                file,
                source: OffsetSource::Directory
            }
        );
    }

    #[tokio::test]
    async fn default_archive_rewind_keeps_versions_and_keep_offset() {
        use leafage_evm_types::{Block, BlockInfo, Header, NewAccount, RawHeader, U256};
        let dir = std::env::temp_dir().join(format!("rewind-default-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = MultiStorage::open(&dir, 16, StorageKind::MDBX, true, false, false).unwrap();
        for n in 1..=2 {
            let block = BlockInfo::new(Block {
                header: Header {
                    hash: H256::repeat_byte(n as u8),
                    inner: RawHeader {
                        number: n,
                        parent_hash: H256::repeat_byte(n as u8 - 1),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            });
            StateDBWrapper(db.db_at(BlockId::latest()).unwrap().unwrap())
                .update_block(
                    block,
                    BlockStorageDiff {
                        new_accounts: vec![NewAccount {
                            address: H256::repeat_byte(10),
                            balance: U256::from(n),
                            nonce: n,
                            code_hash: H256::ZERO,
                        }],
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        drop(db);
        let mut a = args();
        a.extend(["--archive", "--keep-offset"]);
        let mut cmd = Command::try_parse_from(a).unwrap();
        cmd.db_path = dir.clone();
        cmd.db_type = StorageKind::MDBX;
        std::fs::create_dir_all(dir.join("offset")).unwrap();
        std::fs::write(dir.join("offset/offset"), "123").unwrap();
        cmd.run().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("offset/offset")).unwrap(),
            "123"
        );
        let db = MultiStorage::open(&dir, 16, StorageKind::MDBX, true, false, false).unwrap();
        assert_eq!(
            db.db_at(BlockId::latest())
                .unwrap()
                .unwrap()
                .read_latest_block_hash()
                .unwrap(),
            H256::repeat_byte(1)
        );
        assert_eq!(
            db.db_at(BlockId::number(2))
                .unwrap()
                .unwrap()
                .read_account(H256::repeat_byte(10))
                .unwrap()
                .unwrap()
                .balance,
            U256::from(2)
        );
        drop(db);
        // Same-height truncation now removes the physical future left by default rewind.
        cmd.keep_offset = false;
        cmd.truncate_archive = true;
        cmd.archive_encoding = Some(ArchiveEncoding::Legacy);
        cmd.offset_dir = Some(dir.join("offset"));
        cmd.run().await.unwrap();
        assert!(!dir.join("offset/offset").exists());
        let db = MultiStorage::open(&dir, 16, StorageKind::MDBX, true, false, false).unwrap();
        assert!(db.db_at(BlockId::number(2)).unwrap().is_none());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
