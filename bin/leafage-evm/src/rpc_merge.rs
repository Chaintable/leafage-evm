//! Build one bridge `StateDiff` from two adjacent RPC state databases.
//!
//! The command is deliberately an offline, single-flow operation:
//!
//! 1. Open the old and new RocksDB RPC databases read-only.
//! 2. Merge their sorted account/storage/code streams into bounded-memory
//!    spool files.
//! 3. Assemble the final RLP in a second pass, once every list length is known.
//! 4. Re-scan the sources and verify the final RLP without materializing the
//!    complete diff in memory.
//! 5. Multipart-upload the file to S3 and stream it back for checksum
//!    verification.
//!
//! The existing RPC-side StateDiff download/decoding path is intentionally out
//! of scope. This command only produces and publishes the bridge artifact.

use crate::utils::{parse_kafka_s3_config, state_diff_keyed_by_block_hash, KafkaS3Config};
use alloy::primitives::B64;
use alloy_rlp::{Decodable, Encodable, Header as RlpHeader};
use anyhow::{anyhow, bail, Context, Result};
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client as S3Client;
use clap::Parser;
use leafage_evm_types::{
    BlockInfo, Bytes, IndexValuePair, NewAccount, NewCode, RawHeader, SlimAccount, H256,
    KECCAK256_EMPTY, U256,
};
use rocksdb::{IteratorMode, Options, ReadOptions, DB};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;
use tracing::{info, warn};

const COLUMN_FAMILIES: [&str; 6] = ["1", "2", "3", "4", "5", "6"];
const LATEST_BLOCK_HASH_CF: &str = "1";
const BLOCK_INFO_CF: &str = "2";
const ACCOUNT_CF: &str = "4";
const STORAGE_CF: &str = "5";
const CODE_CF: &str = "6";
const ENCODING_MARKER_KEY: &[u8] = b"leafage:block_encoding_inverted";
const LATEST_BLOCK_HASH_KEY: [u8; 1] = [1];

const ACCOUNT_SPOOL: &str = "accounts.rlp";
const DELETED_ACCOUNT_SPOOL: &str = "deleted-accounts.rlp";
const STORAGE_SPOOL: &str = "storage.records";
const STORAGE_GROUP_SPOOL: &str = "storage.groups";
const CODE_SPOOL: &str = "codes.rlp";
const ARTIFACT_FILE: &str = "stateDiff.rlp";
const REPORT_FILE: &str = "report.json";

const STORAGE_RECORD_BYTES: usize = 96;
const STORAGE_GROUP_BYTES: usize = 48;
const COPY_BUFFER_BYTES: usize = 1024 * 1024;
const DEFAULT_MULTIPART_PART_BYTES: u64 = 64 * 1024 * 1024;
const MIN_MULTIPART_PART_BYTES: u64 = 5 * 1024 * 1024;
const MAX_MULTIPART_PARTS: u64 = 10_000;

#[derive(Debug, Parser)]
pub struct Command {
    /// RocksDB directory for the last state served by the old RPC.
    #[arg(long, value_name = "PATH")]
    old_db: PathBuf,

    /// RocksDB directory for the first state served by the new RPC.
    #[arg(long, value_name = "PATH")]
    new_db: PathBuf,

    /// First block height served by the new RPC. The old RPC must be at H-1.
    #[arg(long)]
    fork_height: u64,

    /// Existing Kafka/S3 config file (or inline JSON). Only bucket_name,
    /// s3_chain_id and version are used by this command.
    #[arg(long, value_parser = parse_kafka_s3_config, value_name = "S3_CONFIG_PATH")]
    s3_config: KafkaS3Config,

    /// Directory for bounded-memory spool files, final RLP and the report.
    #[arg(long, value_name = "PATH")]
    work_dir: Option<PathBuf>,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        self.validate_arguments()?;

        let work_dir = match &self.work_dir {
            Some(path) => path.clone(),
            None => std::env::current_dir()?.join(format!("rpc-merge-{}", self.fork_height)),
        };
        fs::create_dir_all(&work_dir)
            .with_context(|| format!("create work directory {}", work_dir.display()))?;

        info!(target: "rpc_merge", "opening source RPC databases read-only");
        let old_db = RpcDatabase::open(&self.old_db)?;
        let new_db = RpcDatabase::open(&self.new_db)?;
        let old_anchor = old_db.head()?;
        let new_anchor = new_db.head()?;
        validate_boundary(self.fork_height, &old_anchor, &new_anchor)?;

        info!(
            target: "rpc_merge",
            "boundary accepted: old block={} hash={} state_root={}, new block={} hash={} state_root={}",
            old_anchor.number,
            old_anchor.block_hash,
            old_anchor.state_root,
            new_anchor.number,
            new_anchor.block_hash,
            new_anchor.state_root,
        );

        let artifacts = build_spools(&old_db, &new_db, &work_dir)?;
        verify_sources(&old_db, &new_db, &artifacts)?;
        verify_spool_files(&artifacts)?;

        let artifact_path = work_dir.join(ARTIFACT_FILE);
        let assembled_size = assemble_state_diff(
            &artifact_path,
            old_anchor.state_root,
            new_anchor.state_root,
            &artifacts,
        )?;
        let artifact_sha256 = verify_final_rlp(
            &artifact_path,
            old_anchor.state_root,
            new_anchor.state_root,
            &artifacts,
        )?;

        // A second head read catches accidental use of a live/moving source.
        if old_db.head()? != old_anchor || new_db.head()? != new_anchor {
            bail!("source RPC database head changed while the bridge was being built");
        }

        let diff_key = if state_diff_keyed_by_block_hash(&self.s3_config.s3_chain_id) {
            new_anchor.block_hash
        } else {
            new_anchor.state_root
        };
        let s3_key = state_diff_key(
            &self.s3_config.s3_chain_id,
            &self.s3_config.version,
            diff_key,
        );
        let s3_client = S3Client::new(&aws_config::load_from_env().await);
        let upload = upload_file(
            &s3_client,
            &self.s3_config.bucket_name,
            &s3_key,
            &artifact_path,
            assembled_size,
            &artifact_sha256,
            self.fork_height,
            old_anchor.state_root,
        )
        .await?;
        verify_s3_object(
            &s3_client,
            &self.s3_config.bucket_name,
            &s3_key,
            assembled_size,
            &artifact_sha256,
        )
        .await?;

        let report = MergeReport {
            fork_height: self.fork_height,
            old_db: self.old_db.clone(),
            new_db: self.new_db.clone(),
            old_mode: old_db.mode,
            new_mode: new_db.mode,
            old_anchor,
            new_anchor,
            accounts: artifacts.accounts.stats.clone(),
            deleted_accounts: artifacts.deleted_accounts.stats.clone(),
            storage: artifacts.storage.stats.clone(),
            codes: artifacts.codes.stats.clone(),
            artifact_path: artifact_path.clone(),
            artifact_bytes: assembled_size,
            artifact_sha256: artifact_sha256.clone(),
            s3_bucket: self.s3_config.bucket_name.clone(),
            s3_key: s3_key.clone(),
            uploaded: upload.uploaded,
            multipart_parts: upload.parts,
        };
        write_json_file(&work_dir.join(REPORT_FILE), &report)?;

        info!(
            target: "rpc_merge",
            "bridge complete: file={} bytes={} sha256={} s3://{}/{}",
            artifact_path.display(),
            assembled_size,
            artifact_sha256,
            self.s3_config.bucket_name,
            s3_key,
        );
        Ok(())
    }

    fn validate_arguments(&self) -> Result<()> {
        if self.fork_height == 0 {
            bail!("--fork-height must be greater than zero");
        }
        if self.old_db == self.new_db {
            bail!("--old-db and --new-db must be different directories");
        }
        for (label, path) in [("old", &self.old_db), ("new", &self.new_db)] {
            if !path.is_dir() {
                bail!(
                    "{label} RPC database directory does not exist: {}",
                    path.display()
                );
            }
            if !path.join("CURRENT").is_file() {
                bail!(
                    "{label} RPC database is not a RocksDB directory: {}",
                    path.display()
                );
            }
        }
        if self.s3_config.bucket_name.is_empty() {
            bail!("S3 config bucket_name must not be empty");
        }
        if self.s3_config.s3_chain_id.is_empty() {
            bail!("S3 config s3_chain_id must not be empty");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum DatabaseMode {
    Snapshot,
    ArchiveLegacy,
    ArchiveInverted,
}

impl DatabaseMode {
    fn is_archive(self) -> bool {
        !matches!(self, Self::Snapshot)
    }

    fn is_inverted(self) -> bool {
        matches!(self, Self::ArchiveInverted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Anchor {
    number: u64,
    block_hash: H256,
    parent_hash: H256,
    state_root: H256,
}

struct RpcDatabase {
    db: DB,
    mode: DatabaseMode,
    path: PathBuf,
}

impl RpcDatabase {
    fn open(path: &Path) -> Result<Self> {
        let mut options = Options::default();
        options.set_max_open_files(512);
        let db = DB::open_cf_for_read_only(&options, path, COLUMN_FAMILIES, false)
            .with_context(|| format!("open RocksDB read-only: {}", path.display()))?;
        let mode = detect_database_mode(&db)?;
        info!(target: "rpc_merge", "opened {} as {:?}", path.display(), mode);
        Ok(Self {
            db,
            mode,
            path: path.to_path_buf(),
        })
    }

    fn head(&self) -> Result<Anchor> {
        let latest_cf = self.cf(LATEST_BLOCK_HASH_CF)?;
        let block_hash_bytes = self
            .db
            .get_cf(latest_cf, LATEST_BLOCK_HASH_KEY)?
            .ok_or_else(|| anyhow!("missing latest block hash in {}", self.path.display()))?;
        if block_hash_bytes.len() != 32 {
            bail!(
                "invalid latest block hash length in {}",
                self.path.display()
            );
        }
        let block_hash = H256::from_slice(&block_hash_bytes);
        let block_info_cf = self.cf(BLOCK_INFO_CF)?;
        let raw = self
            .db
            .get_cf(block_info_cf, block_hash.as_slice())?
            .ok_or_else(|| anyhow!("missing latest block info in {}", self.path.display()))?;

        if self.mode == DatabaseMode::Snapshot {
            let block: BlockInfo = serde_json::from_slice(&raw).with_context(|| {
                format!("decode snapshot block info in {}", self.path.display())
            })?;
            Ok(Anchor {
                number: block.header.number,
                block_hash,
                parent_hash: block.header.parent_hash,
                state_root: block.header.state_root,
            })
        } else {
            let header = decode_archive_header(&raw)
                .with_context(|| format!("decode archive block info in {}", self.path.display()))?;
            Ok(Anchor {
                number: header.number,
                block_hash,
                parent_hash: header.parent_hash,
                state_root: header.state_root,
            })
        }
    }

    fn account_iter(&self) -> Result<Box<dyn Iterator<Item = Result<(H256, NewAccount)>> + '_>> {
        let cf = self.cf(ACCOUNT_CF)?;
        let iter = self
            .db
            .iterator_cf_opt(cf, read_options(), IteratorMode::Start);
        if !self.mode.is_archive() {
            return Ok(Box::new(iter.map(|item| {
                let (key, value) = item?;
                if key.len() != 32 {
                    bail!("snapshot account key must be 32 bytes, got {}", key.len());
                }
                let address = H256::from_slice(&key);
                Ok((address, decode_account(address, &value)?))
            })));
        }

        let inverted = self.mode.is_inverted();
        let mut iter = iter.peekable();
        let mut consumed_prefix: Option<[u8; 32]> = None;
        Ok(Box::new(std::iter::from_fn(move || loop {
            let item = iter.next()?;
            let (key, value) = match item {
                Ok(item) => item,
                Err(error) => return Some(Err(error.into())),
            };
            if key.len() != 64 {
                return Some(Err(anyhow!(
                    "archive account key must be 64 bytes, got {}",
                    key.len()
                )));
            }
            let prefix: [u8; 32] = key[..32].try_into().expect("checked key length");
            let sentinel = !inverted && is_legacy_sentinel(&key[32..64]);
            let newest = if inverted {
                let newest = consumed_prefix != Some(prefix);
                consumed_prefix = Some(prefix);
                newest
            } else {
                match iter.peek() {
                    Some(Ok((next_key, _))) if next_key.len() == 64 => {
                        next_key[..32] != prefix || is_legacy_sentinel(&next_key[32..64])
                    }
                    Some(Ok((next_key, _))) => {
                        return Some(Err(anyhow!(
                            "archive account key must be 64 bytes, got {}",
                            next_key.len()
                        )))
                    }
                    Some(Err(_)) | None => true,
                }
            };
            if sentinel || !newest || value.is_empty() {
                continue;
            }
            let address = H256::from(prefix);
            return Some(decode_account(address, &value).map(|account| (address, account)));
        })))
    }

    fn storage_iter(&self) -> Result<Box<dyn Iterator<Item = Result<(H256, H256, U256)>> + '_>> {
        let cf = self.cf(STORAGE_CF)?;
        let iter = self
            .db
            .iterator_cf_opt(cf, read_options(), IteratorMode::Start);
        if !self.mode.is_archive() {
            return Ok(Box::new(iter.map(|item| {
                let (key, value) = item?;
                if key.len() != 64 {
                    bail!("snapshot storage key must be 64 bytes, got {}", key.len());
                }
                let address = H256::from_slice(&key[..32]);
                let index = H256::from_slice(&key[32..64]);
                let value = U256::from_be_slice(&value);
                Ok((address, index, value))
            })));
        }

        let inverted = self.mode.is_inverted();
        let mut iter = iter.peekable();
        let mut consumed_prefix: Option<[u8; 64]> = None;
        Ok(Box::new(std::iter::from_fn(move || loop {
            let item = iter.next()?;
            let (key, value) = match item {
                Ok(item) => item,
                Err(error) => return Some(Err(error.into())),
            };
            if key.len() != 96 {
                return Some(Err(anyhow!(
                    "archive storage key must be 96 bytes, got {}",
                    key.len()
                )));
            }
            let prefix: [u8; 64] = key[..64].try_into().expect("checked key length");
            let sentinel = !inverted && is_legacy_sentinel(&key[64..96]);
            let newest = if inverted {
                let newest = consumed_prefix != Some(prefix);
                consumed_prefix = Some(prefix);
                newest
            } else {
                match iter.peek() {
                    Some(Ok((next_key, _))) if next_key.len() == 96 => {
                        next_key[..64] != prefix || is_legacy_sentinel(&next_key[64..96])
                    }
                    Some(Ok((next_key, _))) => {
                        return Some(Err(anyhow!(
                            "archive storage key must be 96 bytes, got {}",
                            next_key.len()
                        )))
                    }
                    Some(Err(_)) | None => true,
                }
            };
            if sentinel || !newest {
                continue;
            }
            let storage_value = U256::from_be_slice(&value);
            if storage_value == U256::ZERO {
                continue;
            }
            return Some(Ok((
                H256::from_slice(&prefix[..32]),
                H256::from_slice(&prefix[32..64]),
                storage_value,
            )));
        })))
    }

    fn code_iter(&self) -> Result<Box<dyn Iterator<Item = Result<(H256, Bytes)>> + '_>> {
        let cf = self.cf(CODE_CF)?;
        let iter = self
            .db
            .iterator_cf_opt(cf, read_options(), IteratorMode::Start);
        Ok(Box::new(iter.map(|item| {
            let (key, value) = item?;
            if key.len() != 32 {
                bail!("code key must be 32 bytes, got {}", key.len());
            }
            Ok((H256::from_slice(&key), Bytes::from(value)))
        })))
    }

    fn cf(&self, name: &str) -> Result<&rocksdb::ColumnFamily> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| anyhow!("column family {name} not found in {}", self.path.display()))
    }
}

fn detect_database_mode(db: &DB) -> Result<DatabaseMode> {
    let latest_cf = db
        .cf_handle(LATEST_BLOCK_HASH_CF)
        .ok_or_else(|| anyhow!("latest block hash column family is missing"))?;
    if let Some(marker) = db.get_cf(latest_cf, ENCODING_MARKER_KEY)? {
        return Ok(if marker.first().copied() == Some(1) {
            DatabaseMode::ArchiveInverted
        } else {
            DatabaseMode::ArchiveLegacy
        });
    }

    let latest_hash = db
        .get_cf(latest_cf, LATEST_BLOCK_HASH_KEY)?
        .ok_or_else(|| anyhow!("latest block hash is missing"))?;
    let block_cf = db
        .cf_handle(BLOCK_INFO_CF)
        .ok_or_else(|| anyhow!("block info column family is missing"))?;
    let raw = db
        .get_cf(block_cf, latest_hash)?
        .ok_or_else(|| anyhow!("latest block info is missing"))?;
    if serde_json::from_slice::<BlockInfo>(&raw).is_ok() {
        Ok(DatabaseMode::Snapshot)
    } else {
        // Unmarked archive databases predate the inverted encoding.
        Ok(DatabaseMode::ArchiveLegacy)
    }
}

fn decode_archive_header(raw: &[u8]) -> Result<RawHeader> {
    let mut input = raw;
    if let Ok(header) = RawHeader::decode(&mut input) {
        return Ok(header);
    }

    // Compatibility with the older fixed-field header encoding.
    input = raw;
    let header = RlpHeader::decode(&mut input)?;
    if !header.list {
        bail!("archive block header is not an RLP list");
    }
    Ok(RawHeader {
        parent_hash: Decodable::decode(&mut input)?,
        ommers_hash: Decodable::decode(&mut input)?,
        beneficiary: Decodable::decode(&mut input)?,
        state_root: Decodable::decode(&mut input)?,
        transactions_root: Decodable::decode(&mut input)?,
        receipts_root: Decodable::decode(&mut input)?,
        logs_bloom: Decodable::decode(&mut input)?,
        difficulty: Decodable::decode(&mut input)?,
        number: u64::decode(&mut input)?,
        gas_limit: u64::decode(&mut input)?,
        gas_used: u64::decode(&mut input)?,
        timestamp: Decodable::decode(&mut input)?,
        extra_data: Decodable::decode(&mut input)?,
        mix_hash: Decodable::decode(&mut input)?,
        nonce: B64::decode(&mut input)?,
        ..Default::default()
    })
}

fn decode_account(address: H256, raw: &[u8]) -> Result<NewAccount> {
    let mut input = raw;
    let account = SlimAccount::decode(&mut input)?;
    if !input.is_empty() {
        bail!("account {} has trailing bytes", address);
    }
    Ok(NewAccount {
        address,
        balance: account.balance,
        nonce: account.nonce,
        code_hash: if account.code_hash.is_zero() {
            KECCAK256_EMPTY.0.into()
        } else {
            account.code_hash
        },
    })
}

fn is_legacy_sentinel(tail: &[u8]) -> bool {
    tail.len() == 32 && tail[24..32] == u64::MAX.to_be_bytes()
}

fn read_options() -> ReadOptions {
    let mut options = ReadOptions::default();
    options.set_verify_checksums(false);
    options.set_total_order_seek(true);
    options
}

fn validate_boundary(fork_height: u64, old: &Anchor, new: &Anchor) -> Result<()> {
    let expected_old = fork_height
        .checked_sub(1)
        .ok_or_else(|| anyhow!("fork height underflow"))?;
    if old.number != expected_old {
        bail!(
            "old RPC head must be block {}, found {}",
            expected_old,
            old.number
        );
    }
    if new.number != fork_height {
        bail!(
            "new RPC head must be block {}, found {}",
            fork_height,
            new.number
        );
    }
    if new.parent_hash != old.block_hash {
        bail!(
            "boundary is not canonical: new parent {} != old block {}",
            new.parent_hash,
            old.block_hash
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SectionStats {
    records: u64,
    encoded_bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct StorageStats {
    records: u64,
    groups: u64,
    raw_bytes: u64,
    raw_sha256: String,
    group_bytes: u64,
    group_sha256: String,
    rlp_payload_bytes: u64,
}

struct EncodedArtifact {
    path: PathBuf,
    stats: SectionStats,
}

struct StorageArtifact {
    records_path: PathBuf,
    groups_path: PathBuf,
    stats: StorageStats,
}

struct Artifacts {
    accounts: EncodedArtifact,
    deleted_accounts: EncodedArtifact,
    storage: StorageArtifact,
    codes: EncodedArtifact,
}

struct EncodedSpool {
    path: PathBuf,
    writer: BufWriter<File>,
    hasher: Sha256,
    records: u64,
    bytes: u64,
}

impl EncodedSpool {
    fn create(path: PathBuf) -> Result<Self> {
        let file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
        Ok(Self {
            path,
            writer: BufWriter::new(file),
            hasher: Sha256::new(),
            records: 0,
            bytes: 0,
        })
    }

    fn append<T: Encodable>(&mut self, value: &T) -> Result<()> {
        let mut encoded = Vec::with_capacity(value.length());
        value.encode(&mut encoded);
        self.writer.write_all(&encoded)?;
        self.hasher.update(&encoded);
        self.records = self
            .records
            .checked_add(1)
            .context("record count overflow")?;
        self.bytes = self
            .bytes
            .checked_add(u64::try_from(encoded.len())?)
            .context("encoded byte count overflow")?;
        Ok(())
    }

    fn finish(mut self) -> Result<EncodedArtifact> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(EncodedArtifact {
            path: self.path,
            stats: SectionStats {
                records: self.records,
                encoded_bytes: self.bytes,
                sha256: digest_hex(self.hasher),
            },
        })
    }
}

struct StorageSpool {
    records_path: PathBuf,
    groups_path: PathBuf,
    records_writer: BufWriter<File>,
    groups_writer: BufWriter<File>,
    hasher: Sha256,
    groups_hasher: Sha256,
    records: u64,
    groups: u64,
    rlp_payload_bytes: u64,
    current_address: Option<H256>,
    current_count: u64,
    current_pairs_payload: u64,
}

impl StorageSpool {
    fn create(records_path: PathBuf, groups_path: PathBuf) -> Result<Self> {
        let records_file = File::create(&records_path)
            .with_context(|| format!("create {}", records_path.display()))?;
        let groups_file = File::create(&groups_path)
            .with_context(|| format!("create {}", groups_path.display()))?;
        Ok(Self {
            records_path,
            groups_path,
            records_writer: BufWriter::new(records_file),
            groups_writer: BufWriter::new(groups_file),
            hasher: Sha256::new(),
            groups_hasher: Sha256::new(),
            records: 0,
            groups: 0,
            rlp_payload_bytes: 0,
            current_address: None,
            current_count: 0,
            current_pairs_payload: 0,
        })
    }

    fn append(&mut self, address: H256, index: H256, value: U256) -> Result<()> {
        if self
            .current_address
            .is_some_and(|current| current != address)
        {
            self.flush_group()?;
        }
        if self.current_address.is_none() {
            self.current_address = Some(address);
        }

        let value_bytes: [u8; 32] = value.to_be_bytes();
        self.records_writer.write_all(address.as_slice())?;
        self.records_writer.write_all(index.as_slice())?;
        self.records_writer.write_all(&value_bytes)?;
        self.hasher.update(address.as_slice());
        self.hasher.update(index.as_slice());
        self.hasher.update(value_bytes);

        let pair = IndexValuePair { index, value };
        self.current_pairs_payload = self
            .current_pairs_payload
            .checked_add(u64::try_from(pair.length())?)
            .context("storage pair payload overflow")?;
        self.current_count = self
            .current_count
            .checked_add(1)
            .context("group count overflow")?;
        self.records = self
            .records
            .checked_add(1)
            .context("storage count overflow")?;
        Ok(())
    }

    fn flush_group(&mut self) -> Result<()> {
        let Some(address) = self.current_address.take() else {
            return Ok(());
        };
        let mut group = [0u8; STORAGE_GROUP_BYTES];
        group[..32].copy_from_slice(address.as_slice());
        group[32..40].copy_from_slice(&self.current_count.to_be_bytes());
        group[40..48].copy_from_slice(&self.current_pairs_payload.to_be_bytes());
        self.groups_writer.write_all(&group)?;
        self.groups_hasher.update(group);

        let pairs_total = list_total_length(self.current_pairs_payload)?;
        let account_payload = encoded_length(&address)?
            .checked_add(pairs_total)
            .context("storage account payload overflow")?;
        self.rlp_payload_bytes = self
            .rlp_payload_bytes
            .checked_add(list_total_length(account_payload)?)
            .context("storage section payload overflow")?;
        self.groups = self
            .groups
            .checked_add(1)
            .context("storage group overflow")?;
        self.current_count = 0;
        self.current_pairs_payload = 0;
        Ok(())
    }

    fn finish(mut self) -> Result<StorageArtifact> {
        self.flush_group()?;
        self.records_writer.flush()?;
        self.groups_writer.flush()?;
        self.records_writer.get_ref().sync_all()?;
        self.groups_writer.get_ref().sync_all()?;
        Ok(StorageArtifact {
            records_path: self.records_path,
            groups_path: self.groups_path,
            stats: StorageStats {
                records: self.records,
                groups: self.groups,
                raw_bytes: self
                    .records
                    .checked_mul(STORAGE_RECORD_BYTES as u64)
                    .context("storage raw size overflow")?,
                raw_sha256: digest_hex(self.hasher),
                group_bytes: self
                    .groups
                    .checked_mul(STORAGE_GROUP_BYTES as u64)
                    .context("storage group size overflow")?,
                group_sha256: digest_hex(self.groups_hasher),
                rlp_payload_bytes: self.rlp_payload_bytes,
            },
        })
    }
}

fn build_spools(old: &RpcDatabase, new: &RpcDatabase, work_dir: &Path) -> Result<Artifacts> {
    info!(target: "rpc_merge", "streaming account differences");
    let mut accounts = EncodedSpool::create(work_dir.join(ACCOUNT_SPOOL))?;
    let mut deleted = EncodedSpool::create(work_dir.join(DELETED_ACCOUNT_SPOOL))?;
    scan_accounts(
        old.account_iter()?,
        new.account_iter()?,
        |account| accounts.append(account),
        |address| deleted.append(address),
    )?;

    info!(target: "rpc_merge", "streaming storage differences");
    let mut storage = StorageSpool::create(
        work_dir.join(STORAGE_SPOOL),
        work_dir.join(STORAGE_GROUP_SPOOL),
    )?;
    scan_storage(
        old.storage_iter()?,
        new.storage_iter()?,
        |address, index, value| storage.append(address, index, value),
    )?;

    info!(target: "rpc_merge", "streaming code differences");
    let mut codes = EncodedSpool::create(work_dir.join(CODE_SPOOL))?;
    scan_codes(old.code_iter()?, new.code_iter()?, |code| {
        codes.append(code)
    })?;

    let artifacts = Artifacts {
        accounts: accounts.finish()?,
        deleted_accounts: deleted.finish()?,
        storage: storage.finish()?,
        codes: codes.finish()?,
    };
    info!(
        target: "rpc_merge",
        "spools complete: accounts={} deleted={} storage_slots={} storage_accounts={} codes={}",
        artifacts.accounts.stats.records,
        artifacts.deleted_accounts.stats.records,
        artifacts.storage.stats.records,
        artifacts.storage.stats.groups,
        artifacts.codes.stats.records,
    );
    Ok(artifacts)
}

fn scan_accounts<IO, IN, FU, FD>(
    mut old: IO,
    mut new: IN,
    mut update: FU,
    mut delete: FD,
) -> Result<()>
where
    IO: Iterator<Item = Result<(H256, NewAccount)>>,
    IN: Iterator<Item = Result<(H256, NewAccount)>>,
    FU: FnMut(&NewAccount) -> Result<()>,
    FD: FnMut(&H256) -> Result<()>,
{
    let mut old_value = next_item(&mut old)?;
    let mut new_value = next_item(&mut new)?;
    loop {
        match (&old_value, &new_value) {
            (None, None) => break,
            (Some((old_address, _)), None) => {
                delete(old_address)?;
                old_value = next_item(&mut old)?;
            }
            (None, Some((_, new_account))) => {
                update(new_account)?;
                new_value = next_item(&mut new)?;
            }
            (Some((old_address, old_account)), Some((new_address, new_account))) => {
                match old_address.cmp(new_address) {
                    Ordering::Less => {
                        delete(old_address)?;
                        old_value = next_item(&mut old)?;
                    }
                    Ordering::Greater => {
                        update(new_account)?;
                        new_value = next_item(&mut new)?;
                    }
                    Ordering::Equal => {
                        if old_account != new_account {
                            update(new_account)?;
                        }
                        old_value = next_item(&mut old)?;
                        new_value = next_item(&mut new)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn scan_storage<IO, IN, F>(mut old: IO, mut new: IN, mut changed: F) -> Result<()>
where
    IO: Iterator<Item = Result<(H256, H256, U256)>>,
    IN: Iterator<Item = Result<(H256, H256, U256)>>,
    F: FnMut(H256, H256, U256) -> Result<()>,
{
    let mut old_value = next_item(&mut old)?;
    let mut new_value = next_item(&mut new)?;
    loop {
        match (&old_value, &new_value) {
            (None, None) => break,
            (Some((address, index, _)), None) => {
                changed(*address, *index, U256::ZERO)?;
                old_value = next_item(&mut old)?;
            }
            (None, Some((address, index, value))) => {
                changed(*address, *index, *value)?;
                new_value = next_item(&mut new)?;
            }
            (
                Some((old_address, old_index, old_value_inner)),
                Some((new_address, new_index, new_value_inner)),
            ) => match (*old_address, *old_index).cmp(&(*new_address, *new_index)) {
                Ordering::Less => {
                    changed(*old_address, *old_index, U256::ZERO)?;
                    old_value = next_item(&mut old)?;
                }
                Ordering::Greater => {
                    changed(*new_address, *new_index, *new_value_inner)?;
                    new_value = next_item(&mut new)?;
                }
                Ordering::Equal => {
                    if old_value_inner != new_value_inner {
                        changed(*new_address, *new_index, *new_value_inner)?;
                    }
                    old_value = next_item(&mut old)?;
                    new_value = next_item(&mut new)?;
                }
            },
        }
    }
    Ok(())
}

fn scan_codes<IO, IN, F>(mut old: IO, mut new: IN, mut added: F) -> Result<()>
where
    IO: Iterator<Item = Result<(H256, Bytes)>>,
    IN: Iterator<Item = Result<(H256, Bytes)>>,
    F: FnMut(&NewCode) -> Result<()>,
{
    let mut old_value = next_item(&mut old)?;
    let mut new_value = next_item(&mut new)?;
    loop {
        match (&old_value, &new_value) {
            (None, None) => break,
            (Some(_), None) => old_value = next_item(&mut old)?,
            (None, Some((hash, code))) => {
                added(&NewCode {
                    code_hash: *hash,
                    code: code.clone(),
                })?;
                new_value = next_item(&mut new)?;
            }
            (Some((old_hash, old_code)), Some((new_hash, new_code))) => {
                match old_hash.cmp(new_hash) {
                    Ordering::Less => old_value = next_item(&mut old)?,
                    Ordering::Greater => {
                        added(&NewCode {
                            code_hash: *new_hash,
                            code: new_code.clone(),
                        })?;
                        new_value = next_item(&mut new)?;
                    }
                    Ordering::Equal => {
                        if old_code != new_code {
                            bail!("code bytes differ for the same hash {}", old_hash);
                        }
                        old_value = next_item(&mut old)?;
                        new_value = next_item(&mut new)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn next_item<I, T>(iter: &mut I) -> Result<Option<T>>
where
    I: Iterator<Item = Result<T>>,
{
    iter.next().transpose()
}

struct EncodedDigest {
    hasher: Sha256,
    records: u64,
    bytes: u64,
}

impl EncodedDigest {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            records: 0,
            bytes: 0,
        }
    }

    fn append<T: Encodable>(&mut self, value: &T) -> Result<()> {
        let mut encoded = Vec::with_capacity(value.length());
        value.encode(&mut encoded);
        self.hasher.update(&encoded);
        self.records = self
            .records
            .checked_add(1)
            .context("record count overflow")?;
        self.bytes = self
            .bytes
            .checked_add(u64::try_from(encoded.len())?)
            .context("byte count overflow")?;
        Ok(())
    }

    fn finish(self) -> SectionStats {
        SectionStats {
            records: self.records,
            encoded_bytes: self.bytes,
            sha256: digest_hex(self.hasher),
        }
    }
}

struct StorageDigest {
    hasher: Sha256,
    groups_hasher: Sha256,
    records: u64,
    groups: u64,
    rlp_payload_bytes: u64,
    current_address: Option<H256>,
    current_count: u64,
    current_pairs_payload: u64,
}

impl StorageDigest {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            groups_hasher: Sha256::new(),
            records: 0,
            groups: 0,
            rlp_payload_bytes: 0,
            current_address: None,
            current_count: 0,
            current_pairs_payload: 0,
        }
    }

    fn append(&mut self, address: H256, index: H256, value: U256) -> Result<()> {
        if self
            .current_address
            .is_some_and(|current| current != address)
        {
            self.flush_group()?;
        }
        if self.current_address.is_none() {
            self.current_address = Some(address);
        }
        let value_bytes: [u8; 32] = value.to_be_bytes();
        self.hasher.update(address.as_slice());
        self.hasher.update(index.as_slice());
        self.hasher.update(value_bytes);
        self.current_pairs_payload = self
            .current_pairs_payload
            .checked_add(u64::try_from(IndexValuePair { index, value }.length())?)
            .context("storage payload overflow")?;
        self.records = self
            .records
            .checked_add(1)
            .context("storage count overflow")?;
        self.current_count = self
            .current_count
            .checked_add(1)
            .context("storage group count overflow")?;
        Ok(())
    }

    fn flush_group(&mut self) -> Result<()> {
        let Some(address) = self.current_address.take() else {
            return Ok(());
        };
        let mut group = [0u8; STORAGE_GROUP_BYTES];
        group[..32].copy_from_slice(address.as_slice());
        group[32..40].copy_from_slice(&self.current_count.to_be_bytes());
        group[40..48].copy_from_slice(&self.current_pairs_payload.to_be_bytes());
        self.groups_hasher.update(group);
        let account_payload = encoded_length(&address)?
            .checked_add(list_total_length(self.current_pairs_payload)?)
            .context("storage account payload overflow")?;
        self.rlp_payload_bytes = self
            .rlp_payload_bytes
            .checked_add(list_total_length(account_payload)?)
            .context("storage section overflow")?;
        self.groups = self
            .groups
            .checked_add(1)
            .context("storage groups overflow")?;
        self.current_count = 0;
        self.current_pairs_payload = 0;
        Ok(())
    }

    fn finish(mut self) -> Result<StorageStats> {
        self.flush_group()?;
        Ok(StorageStats {
            records: self.records,
            groups: self.groups,
            raw_bytes: self
                .records
                .checked_mul(STORAGE_RECORD_BYTES as u64)
                .context("storage raw size overflow")?,
            raw_sha256: digest_hex(self.hasher),
            group_bytes: self
                .groups
                .checked_mul(STORAGE_GROUP_BYTES as u64)
                .context("storage group size overflow")?,
            group_sha256: digest_hex(self.groups_hasher),
            rlp_payload_bytes: self.rlp_payload_bytes,
        })
    }
}

fn verify_sources(old: &RpcDatabase, new: &RpcDatabase, artifacts: &Artifacts) -> Result<()> {
    info!(target: "rpc_merge", "re-scanning sources for streaming verification");
    let mut accounts = EncodedDigest::new();
    let mut deleted = EncodedDigest::new();
    scan_accounts(
        old.account_iter()?,
        new.account_iter()?,
        |account| accounts.append(account),
        |address| deleted.append(address),
    )?;
    if accounts.finish() != artifacts.accounts.stats {
        bail!("account spool verification failed");
    }
    if deleted.finish() != artifacts.deleted_accounts.stats {
        bail!("deleted-account spool verification failed");
    }

    let mut storage = StorageDigest::new();
    scan_storage(
        old.storage_iter()?,
        new.storage_iter()?,
        |address, index, value| storage.append(address, index, value),
    )?;
    if storage.finish()? != artifacts.storage.stats {
        bail!("storage spool verification failed");
    }

    let mut codes = EncodedDigest::new();
    scan_codes(old.code_iter()?, new.code_iter()?, |code| {
        codes.append(code)
    })?;
    if codes.finish() != artifacts.codes.stats {
        bail!("code spool verification failed");
    }
    Ok(())
}

fn verify_spool_files(artifacts: &Artifacts) -> Result<()> {
    info!(target: "rpc_merge", "stream-verifying persisted spool files");
    verify_encoded_spool_file("account", &artifacts.accounts)?;
    verify_encoded_spool_file("deleted-account", &artifacts.deleted_accounts)?;
    verify_encoded_spool_file("code", &artifacts.codes)?;
    verify_file(
        "storage record",
        &artifacts.storage.records_path,
        artifacts.storage.stats.raw_bytes,
        &artifacts.storage.stats.raw_sha256,
    )?;
    verify_file(
        "storage group",
        &artifacts.storage.groups_path,
        artifacts.storage.stats.group_bytes,
        &artifacts.storage.stats.group_sha256,
    )
}

fn verify_encoded_spool_file(label: &str, artifact: &EncodedArtifact) -> Result<()> {
    verify_file(
        label,
        &artifact.path,
        artifact.stats.encoded_bytes,
        &artifact.stats.sha256,
    )
}

fn verify_file(label: &str, path: &Path, expected_size: u64, expected_sha256: &str) -> Result<()> {
    let actual_size = fs::metadata(path)
        .with_context(|| format!("read {label} spool metadata: {}", path.display()))?
        .len();
    if actual_size != expected_size {
        bail!("{label} spool size mismatch: expected {expected_size}, got {actual_size}");
    }
    let actual_sha256 = sha256_file(path)?;
    if actual_sha256 != expected_sha256 {
        bail!("{label} spool checksum mismatch: expected {expected_sha256}, got {actual_sha256}");
    }
    Ok(())
}

fn assemble_state_diff(
    output_path: &Path,
    parent_state_root: H256,
    state_root: H256,
    artifacts: &Artifacts,
) -> Result<u64> {
    info!(target: "rpc_merge", "assembling final RLP in a second streaming pass");
    let top_payload = [
        encoded_length(&state_root)?,
        encoded_length(&parent_state_root)?,
        list_total_length(artifacts.accounts.stats.encoded_bytes)?,
        list_total_length(artifacts.deleted_accounts.stats.encoded_bytes)?,
        list_total_length(artifacts.storage.stats.rlp_payload_bytes)?,
        list_total_length(artifacts.codes.stats.encoded_bytes)?,
    ]
    .into_iter()
    .try_fold(0u64, |total, value| {
        total.checked_add(value).context("top-level RLP overflow")
    })?;

    let file = File::create(output_path)
        .with_context(|| format!("create final artifact {}", output_path.display()))?;
    let mut writer = BufWriter::new(file);
    write_list_header(&mut writer, top_payload)?;
    write_encoded(&mut writer, &state_root)?;
    write_encoded(&mut writer, &parent_state_root)?;
    write_spool_list(&mut writer, &artifacts.accounts)?;
    write_spool_list(&mut writer, &artifacts.deleted_accounts)?;
    write_storage_list(&mut writer, &artifacts.storage)?;
    write_spool_list(&mut writer, &artifacts.codes)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;

    let expected = list_total_length(top_payload)?;
    let actual = fs::metadata(output_path)?.len();
    if actual != expected {
        bail!("assembled RLP size mismatch: expected {expected}, got {actual}");
    }
    Ok(actual)
}

fn write_spool_list(writer: &mut BufWriter<File>, artifact: &EncodedArtifact) -> Result<()> {
    write_list_header(writer, artifact.stats.encoded_bytes)?;
    copy_file_exact(&artifact.path, writer, artifact.stats.encoded_bytes)
}

fn write_storage_list(writer: &mut BufWriter<File>, artifact: &StorageArtifact) -> Result<()> {
    write_list_header(writer, artifact.stats.rlp_payload_bytes)?;
    let mut groups = BufReader::new(File::open(&artifact.groups_path)?);
    let mut records = BufReader::new(File::open(&artifact.records_path)?);
    let mut group_count = 0u64;
    let mut record_count = 0u64;
    while let Some(group) = read_storage_group(&mut groups)? {
        let pairs_total = list_total_length(group.pairs_payload)?;
        let account_payload = encoded_length(&group.address)?
            .checked_add(pairs_total)
            .context("storage account RLP overflow")?;
        write_list_header(writer, account_payload)?;
        write_encoded(writer, &group.address)?;
        write_list_header(writer, group.pairs_payload)?;
        for _ in 0..group.records {
            let record = read_storage_record(&mut records)?
                .ok_or_else(|| anyhow!("storage record spool ended inside a group"))?;
            if record.address != group.address {
                bail!("storage group address does not match record address");
            }
            write_encoded(
                writer,
                &IndexValuePair {
                    index: record.index,
                    value: record.value,
                },
            )?;
            record_count = record_count
                .checked_add(1)
                .context("storage count overflow")?;
        }
        group_count = group_count
            .checked_add(1)
            .context("storage groups overflow")?;
    }
    if read_storage_record(&mut records)?.is_some() {
        bail!("storage record spool has records not referenced by a group");
    }
    if group_count != artifact.stats.groups || record_count != artifact.stats.records {
        bail!("storage spool counters changed during assembly");
    }
    Ok(())
}

fn verify_final_rlp(
    path: &Path,
    parent_state_root: H256,
    state_root: H256,
    artifacts: &Artifacts,
) -> Result<String> {
    info!(target: "rpc_merge", "stream-verifying final RLP");
    let file_size = fs::metadata(path)?.len();
    let mut reader = BufReader::new(File::open(path)?);
    let top = read_list_header(&mut reader)?;
    if top.payload
        != file_size
            .checked_sub(top.header_bytes)
            .context("invalid RLP size")?
    {
        bail!("top-level RLP payload length does not match file size");
    }
    read_expected_encoded(&mut reader, &state_root)?;
    read_expected_encoded(&mut reader, &parent_state_root)?;
    verify_spool_list(&mut reader, &artifacts.accounts)?;
    verify_spool_list(&mut reader, &artifacts.deleted_accounts)?;
    verify_storage_list(&mut reader, &artifacts.storage)?;
    verify_spool_list(&mut reader, &artifacts.codes)?;
    if reader.stream_position()? != file_size {
        bail!("final RLP has trailing or unconsumed bytes");
    }
    sha256_file(path)
}

fn verify_spool_list(reader: &mut BufReader<File>, artifact: &EncodedArtifact) -> Result<()> {
    let header = read_list_header(reader)?;
    if header.payload != artifact.stats.encoded_bytes {
        bail!("RLP section payload length does not match its spool");
    }
    compare_reader_with_file(reader, &artifact.path, header.payload)
}

fn verify_storage_list(reader: &mut BufReader<File>, artifact: &StorageArtifact) -> Result<()> {
    let outer = read_list_header(reader)?;
    if outer.payload != artifact.stats.rlp_payload_bytes {
        bail!("storage RLP payload length mismatch");
    }
    let mut groups = BufReader::new(File::open(&artifact.groups_path)?);
    let mut records = BufReader::new(File::open(&artifact.records_path)?);
    let mut group_count = 0u64;
    let mut record_count = 0u64;
    while let Some(group) = read_storage_group(&mut groups)? {
        let account_payload = encoded_length(&group.address)?
            .checked_add(list_total_length(group.pairs_payload)?)
            .context("storage account payload overflow")?;
        if read_list_header(reader)?.payload != account_payload {
            bail!("storage account RLP length mismatch");
        }
        read_expected_encoded(reader, &group.address)?;
        if read_list_header(reader)?.payload != group.pairs_payload {
            bail!("storage pair-list RLP length mismatch");
        }
        for _ in 0..group.records {
            let record = read_storage_record(&mut records)?
                .ok_or_else(|| anyhow!("storage record spool ended inside a group"))?;
            if record.address != group.address {
                bail!("storage group address does not match its record");
            }
            read_expected_encoded(
                reader,
                &IndexValuePair {
                    index: record.index,
                    value: record.value,
                },
            )?;
            record_count = record_count
                .checked_add(1)
                .context("storage count overflow")?;
        }
        group_count = group_count
            .checked_add(1)
            .context("storage groups overflow")?;
    }
    if read_storage_record(&mut records)?.is_some() {
        bail!("unreferenced storage record found during verification");
    }
    if group_count != artifact.stats.groups || record_count != artifact.stats.records {
        bail!("storage counters differ during final verification");
    }
    Ok(())
}

#[derive(Debug)]
struct StorageGroup {
    address: H256,
    records: u64,
    pairs_payload: u64,
}

#[derive(Debug)]
struct StorageRecord {
    address: H256,
    index: H256,
    value: U256,
}

fn read_storage_group(reader: &mut BufReader<File>) -> Result<Option<StorageGroup>> {
    let Some(raw) = read_fixed_or_eof::<STORAGE_GROUP_BYTES>(reader)? else {
        return Ok(None);
    };
    Ok(Some(StorageGroup {
        address: H256::from_slice(&raw[..32]),
        records: u64::from_be_bytes(raw[32..40].try_into().expect("fixed group size")),
        pairs_payload: u64::from_be_bytes(raw[40..48].try_into().expect("fixed group size")),
    }))
}

fn read_storage_record(reader: &mut BufReader<File>) -> Result<Option<StorageRecord>> {
    let Some(raw) = read_fixed_or_eof::<STORAGE_RECORD_BYTES>(reader)? else {
        return Ok(None);
    };
    Ok(Some(StorageRecord {
        address: H256::from_slice(&raw[..32]),
        index: H256::from_slice(&raw[32..64]),
        value: U256::from_be_slice(&raw[64..96]),
    }))
}

fn read_fixed_or_eof<const N: usize>(reader: &mut BufReader<File>) -> Result<Option<[u8; N]>> {
    let mut raw = [0u8; N];
    match reader.read(&mut raw[..1])? {
        0 => Ok(None),
        1 => {
            reader.read_exact(&mut raw[1..])?;
            Ok(Some(raw))
        }
        _ => unreachable!("one-byte read returned more than one byte"),
    }
}

#[derive(Debug)]
struct StreamListHeader {
    payload: u64,
    header_bytes: u64,
}

fn read_list_header<R: Read>(reader: &mut R) -> Result<StreamListHeader> {
    let mut first = [0u8; 1];
    reader.read_exact(&mut first)?;
    match first[0] {
        byte @ 0xc0..=0xf7 => Ok(StreamListHeader {
            payload: u64::from(byte - 0xc0),
            header_bytes: 1,
        }),
        byte @ 0xf8..=0xff => {
            let length_bytes = usize::from(byte - 0xf7);
            let mut raw = [0u8; 8];
            reader.read_exact(&mut raw[8 - length_bytes..])?;
            let payload = u64::from_be_bytes(raw);
            if payload < 56 {
                bail!("non-canonical long RLP list header");
            }
            Ok(StreamListHeader {
                payload,
                header_bytes: 1 + u64::try_from(length_bytes)?,
            })
        }
        byte => bail!("expected RLP list header, got 0x{byte:02x}"),
    }
}

fn read_expected_encoded<R: Read, T: Encodable>(reader: &mut R, value: &T) -> Result<()> {
    let mut expected = Vec::with_capacity(value.length());
    value.encode(&mut expected);
    let mut actual = vec![0u8; expected.len()];
    reader.read_exact(&mut actual)?;
    if actual != expected {
        bail!("final RLP item differs from the generated spool data");
    }
    Ok(())
}

fn write_list_header<W: Write>(writer: &mut W, payload: u64) -> Result<()> {
    let payload = usize::try_from(payload).context("RLP payload does not fit usize")?;
    let header = RlpHeader {
        list: true,
        payload_length: payload,
    };
    let mut encoded = Vec::with_capacity(header.length());
    header.encode(&mut encoded);
    writer.write_all(&encoded)?;
    Ok(())
}

fn write_encoded<W: Write, T: Encodable>(writer: &mut W, value: &T) -> Result<()> {
    let mut encoded = Vec::with_capacity(value.length());
    value.encode(&mut encoded);
    writer.write_all(&encoded)?;
    Ok(())
}

fn encoded_length<T: Encodable>(value: &T) -> Result<u64> {
    Ok(u64::try_from(value.length())?)
}

fn list_total_length(payload: u64) -> Result<u64> {
    let payload_usize = usize::try_from(payload).context("RLP payload does not fit usize")?;
    let header = RlpHeader {
        list: true,
        payload_length: payload_usize,
    };
    payload
        .checked_add(u64::try_from(header.length())?)
        .context("RLP list length overflow")
}

fn copy_file_exact<W: Write>(path: &Path, writer: &mut W, expected: u64) -> Result<()> {
    let mut reader = BufReader::with_capacity(COPY_BUFFER_BYTES, File::open(path)?);
    let copied = std::io::copy(&mut reader, writer)?;
    if copied != expected {
        bail!("spool {} changed size during assembly", path.display());
    }
    Ok(())
}

fn compare_reader_with_file<R: Read>(reader: &mut R, path: &Path, length: u64) -> Result<()> {
    let mut expected = BufReader::with_capacity(COPY_BUFFER_BYTES, File::open(path)?);
    let mut remaining = length;
    let mut actual_buf = vec![0u8; COPY_BUFFER_BYTES];
    let mut expected_buf = vec![0u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let take = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64))?;
        reader.read_exact(&mut actual_buf[..take])?;
        expected.read_exact(&mut expected_buf[..take])?;
        if actual_buf[..take] != expected_buf[..take] {
            bail!("final RLP section differs from spool {}", path.display());
        }
        remaining -= u64::try_from(take)?;
    }
    let mut extra = [0u8; 1];
    if expected.read(&mut extra)? != 0 {
        bail!("spool {} grew during verification", path.display());
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut reader = BufReader::with_capacity(COPY_BUFFER_BYTES, File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(digest_hex(hasher))
}

fn digest_hex(hasher: Sha256) -> String {
    alloy::hex::encode(hasher.finalize())
}

fn state_diff_key(chain_id: &str, version: &str, diff_key: H256) -> String {
    if version.is_empty() {
        format!("{chain_id}/{diff_key}/stateDiff")
    } else {
        format!("{chain_id}/{version}/{diff_key}/stateDiff")
    }
}

#[derive(Debug)]
struct UploadResult {
    uploaded: bool,
    parts: u64,
}

async fn upload_file(
    client: &S3Client,
    bucket: &str,
    key: &str,
    path: &Path,
    file_size: u64,
    sha256: &str,
    fork_height: u64,
    parent_state_root: H256,
) -> Result<UploadResult> {
    match client.head_object().bucket(bucket).key(key).send().await {
        Ok(existing) => {
            let existing_sha = existing
                .metadata()
                .and_then(|metadata| metadata.get("sha256"));
            let existing_length = existing
                .content_length()
                .and_then(|length| u64::try_from(length).ok());
            if existing_sha.is_some_and(|value| value == sha256)
                && existing_length == Some(file_size)
            {
                info!(target: "rpc_merge", "S3 object already exists with the same checksum; upload is idempotent");
                return Ok(UploadResult {
                    uploaded: false,
                    parts: 0,
                });
            }
            bail!("S3 object s3://{bucket}/{key} already exists with different content");
        }
        Err(error)
            if error
                .as_service_error()
                .is_some_and(|error| error.is_not_found()) => {}
        Err(error) => return Err(error).context("check destination S3 object"),
    }

    let part_size = multipart_part_size(file_size)?;
    let create = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .metadata("sha256", sha256)
        .metadata("bridge-fork-height", fork_height.to_string())
        .metadata("bridge-parent-state-root", parent_state_root.to_string())
        .send()
        .await
        .context("create S3 multipart upload")?;
    let upload_id = create
        .upload_id()
        .ok_or_else(|| anyhow!("S3 did not return a multipart upload id"))?
        .to_owned();

    let upload_result: Result<Vec<CompletedPart>> = async {
        let mut completed = Vec::new();
        let mut offset = 0u64;
        let mut part_number = 1i32;
        while offset < file_size {
            let length = (file_size - offset).min(part_size);
            let body = ByteStream::read_from()
                .path(path)
                .offset(offset)
                .length(Length::Exact(length))
                .buffer_size(COPY_BUFFER_BYTES)
                .build()
                .await
                .with_context(|| format!("open upload range at byte {offset}"))?;
            let result = client
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .part_number(part_number)
                .content_length(i64::try_from(length)?)
                .body(body)
                .send()
                .await
                .with_context(|| format!("upload S3 part {part_number}"))?;
            let e_tag = result
                .e_tag()
                .ok_or_else(|| anyhow!("S3 part {part_number} has no ETag"))?;
            completed.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(e_tag)
                    .build(),
            );
            info!(target: "rpc_merge", "uploaded part {} ({} bytes)", part_number, length);
            offset = offset
                .checked_add(length)
                .context("upload offset overflow")?;
            part_number = part_number.checked_add(1).context("part number overflow")?;
        }
        Ok(completed)
    }
    .await;

    let completed = match upload_result {
        Ok(parts) => parts,
        Err(error) => {
            warn!(target: "rpc_merge", "multipart upload failed; aborting upload id {}", upload_id);
            let _ = client
                .abort_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .send()
                .await;
            return Err(error);
        }
    };
    let part_count = u64::try_from(completed.len())?;
    let multipart = CompletedMultipartUpload::builder()
        .set_parts(Some(completed))
        .build();
    if let Err(error) = client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(multipart)
        .send()
        .await
    {
        let _ = client
            .abort_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .send()
            .await;
        return Err(error).context("complete S3 multipart upload");
    }
    Ok(UploadResult {
        uploaded: true,
        parts: part_count,
    })
}

fn multipart_part_size(file_size: u64) -> Result<u64> {
    let minimum_for_part_count = file_size
        .checked_add(MAX_MULTIPART_PARTS - 1)
        .context("file size overflow")?
        / MAX_MULTIPART_PARTS;
    let mut size = DEFAULT_MULTIPART_PART_BYTES
        .max(MIN_MULTIPART_PART_BYTES)
        .max(minimum_for_part_count);
    let mib = 1024 * 1024u64;
    size = size.checked_add(mib - 1).context("part size overflow")? / mib * mib;
    let parts = file_size
        .checked_add(size - 1)
        .context("part count overflow")?
        / size;
    if parts > MAX_MULTIPART_PARTS {
        bail!("artifact requires more than {MAX_MULTIPART_PARTS} S3 parts");
    }
    Ok(size)
}

async fn verify_s3_object(
    client: &S3Client,
    bucket: &str,
    key: &str,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<()> {
    info!(target: "rpc_merge", "streaming S3 object back for checksum verification");
    let response = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .context("read uploaded S3 object")?;
    let mut reader = response.body.into_async_read();
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(u64::try_from(read)?)
            .context("download size overflow")?;
    }
    let sha256 = digest_hex(hasher);
    if size != expected_size || sha256 != expected_sha256 {
        bail!(
            "uploaded S3 object verification failed: expected bytes={} sha256={}, got bytes={} sha256={}",
            expected_size,
            expected_sha256,
            size,
            sha256
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct MergeReport {
    fork_height: u64,
    old_db: PathBuf,
    new_db: PathBuf,
    old_mode: DatabaseMode,
    new_mode: DatabaseMode,
    old_anchor: Anchor,
    new_anchor: Anchor,
    accounts: SectionStats,
    deleted_accounts: SectionStats,
    storage: StorageStats,
    codes: SectionStats,
    artifact_path: PathBuf,
    artifact_bytes: u64,
    artifact_sha256: String,
    s3_bucket: String,
    s3_key: String,
    uploaded: bool,
    multipart_parts: u64,
}

fn write_json_file(path: &Path, value: &impl Serialize) -> Result<()> {
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}
