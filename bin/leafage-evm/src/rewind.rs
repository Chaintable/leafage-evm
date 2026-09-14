use crate::utils::{parse_kafka_s3_config, s3_get_block_info_by_number, KafkaS3Config};
use anyhow::{anyhow, bail, Result};
use clap::Parser;
use jsonrpsee::http_client::HttpClientBuilder;
use leafage_evm_storage::{
    EvmStorageWrite, MultiStorage, StateDBProvider, StateDBRead, StateDBWrapper, StorageKind,
};
use leafage_evm_types::{BlockId, BlockNumberOrTag, BlockStorageDiff, H256};
use std::path::PathBuf;
use std::time::Duration;
use tracing::info;

/// `leafage-evm rewind` command
///
/// Rewind to an earlier block so the next `standalone` start resyncs
/// `to_block + 1 ..= head` from S3.
///
/// Snapshot mode: the target block is resolved via --kafka-s3-config or
/// --rpc-addr (one is required), and until the replay catches up the
/// "latest" state is a mixture of old and replayed values. This mode does
/// not remove stale fork state; rebuild a polluted snapshot from a repaired
/// archive before serving it.
///
/// Archive mode resolves the target locally and deletes future versions by
/// default. Use --head-only to keep those versions and only move the head.
/// Stop all database users first; truncation has no checkpoint or automatic
/// interruption recovery.
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

    /// Only move the archive head, retaining future state and block indexes.
    /// Default archive rewind deletes those records in place (offline only).
    #[arg(long, requires = "archive")]
    head_only: bool,

    /// Use inverted block-height keys for an unmarked archive (default: legacy).
    /// A stored encoding marker takes precedence; truncation rejects conflicts.
    #[arg(long, default_value_t = false, requires = "archive")]
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

    /// Keep the kafka offset file.
    /// Default: false
    ///
    /// By default the offset file is deleted so the next start falls back to
    /// the S3 catch-up path. A retained offset would resume Kafka at a
    /// position whose parent blocks no longer match the rewound head, making
    /// every update fail with ParentBlockHashNotFound.
    /// In archive mode, this requires --head-only.
    #[arg(long, default_value_t = false)]
    keep_offset: bool,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        if self.archive && !self.head_only {
            if self.keep_offset {
                bail!("archive truncation forbids --keep-offset; use --head-only to only move the head");
            }
            let offset_file = self.offset_file();
            let db =
                MultiStorage::open_for_archive_rewind(&self.db_path, self.db_cache, self.db_type)?;
            let target = db.rewind_archive(
                self.to_block,
                self.inverted_block_encoding.then_some(true),
                std::path::Path::new(&offset_file),
            )?;
            info!(target: "rewind", number = target.header.number, hash = %target.header.hash, "archive truncation complete");
            return Ok(());
        }
        if self.archive {
            leafage_evm_storage::set_inverted_block_encoding(self.inverted_block_encoding);
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
                Duration::from_secs(cfg.s3_read_timeout_secs.get()),
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
            let offset_file = self.offset_file();
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

    fn offset_file(&self) -> String {
        let offset_dir = match &self.kafka_s3_config {
            Some(cfg) if !cfg.offset_dir.is_empty() => cfg.offset_dir.clone(),
            _ => format!("{}/offset", self.db_path.to_str().unwrap_or_default()),
        };
        format!("{}/offset", offset_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<&'static str> {
        vec!["rewind", "--db-path", "/unused", "--to-block", "1"]
    }

    #[test]
    fn archive_options_default_to_truncation() {
        let mut a = args();
        a.push("--archive");
        let cmd = Command::try_parse_from(a).unwrap();
        assert!(!cmd.head_only);
        assert!(!cmd.inverted_block_encoding);

        let mut a = args();
        a.extend([
            "--archive",
            "--head-only",
            "--keep-offset",
            "--inverted-block-encoding",
        ]);
        let cmd = Command::try_parse_from(a).unwrap();
        assert!(cmd.head_only && cmd.keep_offset && cmd.inverted_block_encoding);

        for option in [
            "--head-only",
            "--inverted-block-encoding",
            "--truncate-archive",
        ] {
            let mut a = args();
            a.push(option);
            assert!(Command::try_parse_from(a).is_err());
        }
        let mut a = args();
        a.extend(["--archive", "--archive-encoding", "legacy"]);
        assert!(Command::try_parse_from(a).is_err());
        let mut a = args();
        a.push("--keep-offset");
        let cmd = Command::try_parse_from(a).unwrap();
        assert!(!cmd.archive && cmd.keep_offset);
    }

    #[tokio::test]
    async fn archive_truncation_rejects_keep_offset_before_opening() {
        let mut a = args();
        a.extend(["--archive", "--keep-offset"]);
        let error = Command::try_parse_from(a).unwrap().run().await.unwrap_err();
        assert!(error.to_string().contains("forbids --keep-offset"));
    }

    #[test]
    fn rewind_modes_share_existing_offset_paths() {
        for head_only in [false, true] {
            let mut a = args();
            a.push("--archive");
            if head_only {
                a.push("--head-only");
            }
            let mut cmd = Command::try_parse_from(a).unwrap();
            assert_eq!(cmd.offset_file(), "/unused/offset/offset");
            cmd.db_path = PathBuf::from("relative-db");
            assert_eq!(cmd.offset_file(), "relative-db/offset/offset");
            cmd.kafka_s3_config = Some(KafkaS3Config::default());
            assert_eq!(cmd.offset_file(), "relative-db/offset/offset");
            for dir in ["/custom-offset", "relative-offset"] {
                cmd.kafka_s3_config.as_mut().unwrap().offset_dir = dir.into();
                assert_eq!(cmd.offset_file(), format!("{dir}/offset"));
            }
        }
    }

    #[tokio::test]
    async fn archive_head_only_then_default_truncation() {
        use leafage_evm_types::{Block, BlockInfo, Header, NewAccount, RawHeader, U256};
        for (kind, inverted, marked) in [
            (StorageKind::MDBX, false, false),
            (StorageKind::MDBX, true, false),
            (StorageKind::Rocksdb, false, false),
            (StorageKind::Rocksdb, true, false),
            (StorageKind::Rocksdb, false, true),
            (StorageKind::Rocksdb, true, true),
        ] {
            leafage_evm_storage::set_inverted_block_encoding(inverted);
            let dir = std::env::temp_dir().join(format!(
                "rewind-cli-{kind:?}-{inverted}-{marked}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let db = MultiStorage::open(&dir, 16, kind, true, false, false).unwrap();
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
            if marked {
                let MultiStorage::RocksDBArchive(raw) = &db else {
                    unreachable!()
                };
                raw.write_encoding_marker(inverted).unwrap();
            }
            drop(db);
            let mut a = args();
            a.extend(["--archive", "--head-only", "--keep-offset"]);
            if inverted && !marked {
                a.push("--inverted-block-encoding");
            }
            let mut cmd = Command::try_parse_from(a).unwrap();
            cmd.db_path = dir.clone();
            cmd.db_type = kind;
            cmd.db_cache = 16;
            std::fs::create_dir_all(dir.join("offset")).unwrap();
            std::fs::write(dir.join("offset/offset"), "123").unwrap();
            cmd.run().await.unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.join("offset/offset")).unwrap(),
                "123"
            );
            let db = MultiStorage::open(&dir, 16, kind, true, false, false).unwrap();
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
            // Parse the default archive command afresh; H=C must remove head-only leftovers.
            let mut a = args();
            a.push("--archive");
            if inverted && !marked {
                a.push("--inverted-block-encoding");
            }
            let mut cmd = Command::try_parse_from(a).unwrap();
            cmd.db_path = dir.clone();
            cmd.db_type = kind;
            cmd.db_cache = 16;
            if marked && !inverted {
                cmd.inverted_block_encoding = true;
                assert!(cmd
                    .run()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("conflicts"));
                assert_eq!(
                    std::fs::read_to_string(dir.join("offset/offset")).unwrap(),
                    "123"
                );
                let db = MultiStorage::open(&dir, 16, kind, true, false, false).unwrap();
                assert!(db.db_at(BlockId::number(2)).unwrap().is_some());
                drop(db);
                cmd.inverted_block_encoding = false;
            }
            // Neither a preceding open nor another archive may supply the encoding.
            leafage_evm_storage::set_inverted_block_encoding(!inverted);
            cmd.run().await.unwrap();
            assert!(!dir.join("offset/offset").exists());
            let db = MultiStorage::open(&dir, 16, kind, true, false, false).unwrap();
            assert!(db.db_at(BlockId::number(2)).unwrap().is_none());
            assert_eq!(
                db.db_at(BlockId::latest())
                    .unwrap()
                    .unwrap()
                    .read_account(H256::repeat_byte(10))
                    .unwrap()
                    .unwrap()
                    .balance,
                U256::from(1)
            );
            drop(db);
            std::fs::remove_dir_all(dir).unwrap();
        }
        leafage_evm_storage::set_inverted_block_encoding(false);
    }
}
