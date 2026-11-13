//! `leafage-evm archive-import-postgres`: seed an archive's first block
//! (block 0 on a ZKsync chain) from a node's Postgres database instead of S3.
//!
//! Some chains never published a `stateDiff` object for block 0: its state is
//! the genesis write set, which is far too large for a single object (the Lens
//! miniblock 0 holds ~3.8e8 storage writes). The node's database, however,
//! holds exactly the inputs the producer used, so this command reconstructs
//! the block the same way `zksync_types::debank::build_state_diff` does:
//!
//! * every `storage_logs` row of the miniblock is a slot write, stored under
//!   `keccak256(address)` / `keccak256(slot)` — the same keys
//!   `EvmStorageWrapper::storage_ref` reads;
//! * every `factory_deps` row is new bytecode, stored by code hash;
//! * unused: `new_accounts` / `deleted_accounts`, which the ZKsync producer
//!   leaves empty — an Era account is derived from system-contract slots.
//!
//! Rows are streamed through a Postgres cursor with parallel query disabled,
//! so they arrive in heap order. For an append-only table that is the order
//! the node wrote them, which reproduces the live path's "last write of a
//! slot wins" without buffering the block in memory or sorting it on disk.
//!
//! After a run the archive holds exactly one block and its `latest` pointer,
//! so `archive-init` resumes at `--block + 1`. Because the state diff carries
//! block hashes rather than state roots, that follow-up run must be given
//! `--statediff-key block-hash`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::keccak256;
use anyhow::{bail, Context, Result};
use leafage_evm_storage::{ArchiveRocksDBStorage, ArchiveSchema, StateDBWrite};
use leafage_evm_types::{BlockInfo, Bytes, H256, U256};
use tracing::{info, warn};

/// `keccak256(rlp([]))`, the header field every Era debank header carries.
const EMPTY_UNCLES: &str = "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347";

/// The zero `miner` of a debank header (20 bytes).
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

/// `leafage-evm archive-import-postgres` command
#[derive(Debug, clap::Parser)]
pub struct Command {
    /// Archive database directory; created when it holds no database yet.
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// Postgres URL of the node that holds the block
    /// (e.g. `postgres://postgres:pw@127.0.0.1:5430/zksync`).
    #[arg(long, env = "DATABASE_URL", value_name = "URL")]
    postgres_url: String,

    /// Miniblock to write. It must be the archive's first block or the
    /// immediate successor of its head.
    #[arg(long, default_value_t = 0)]
    block: u64,

    /// RocksDB block cache, in MB.
    #[arg(long, default_value_t = 1024)]
    db_cache: usize,

    /// Schema of a new RocksDB archive: v1 (ascending block tail), v2
    /// (inverted tail) or v3 (RocksDB user-defined timestamps). A resumed
    /// archive keeps its own schema.
    #[arg(long, default_value = "v3")]
    archive_schema: ArchiveSchema,

    /// Write account/storage keys with the inverted (descending) block-height
    /// encoding. Must match the `--inverted-block-encoding` flag the serving
    /// node runs with.
    #[arg(long, default_value = "false")]
    inverted_block_encoding: bool,

    /// Storage versions buffered before each commit.
    #[arg(long, default_value_t = 200_000)]
    batch_entries: usize,

    /// Rows per `FETCH` from the Postgres cursor.
    #[arg(long, default_value_t = 20_000)]
    fetch_rows: u64,
}

#[derive(Debug, Clone)]
struct MiniblockMeta {
    number: u64,
    hash: H256,
    parent_hash: H256,
    timestamp: u64,
    base_fee: U256,
    gas_limit: u64,
    gas_used: u64,
    logs_bloom: Vec<u8>,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        if self.batch_entries == 0 {
            bail!("--batch-entries must be greater than 0");
        }
        if self.fetch_rows == 0 {
            bail!("--fetch-rows must be greater than 0");
        }

        let archive = self.open_archive()?;

        let (mut client, connection) =
            tokio_postgres::connect(&self.postgres_url, tokio_postgres::NoTls)
                .await
                .context("connect to the node's Postgres")?;
        let connection_task = tokio::spawn(async move {
            if let Err(e) = connection.await {
                warn!("Postgres connection closed: {e}");
            }
        });

        let meta = self.fetch_miniblock(&client).await?;
        info!(
            "importing block {} ({:#x}) into {:?}",
            meta.number, meta.hash, self.db_path
        );
        let block_info = Self::build_block_info(&meta)?;

        let outcome = self.copy_state(&archive, &mut client, &meta).await;
        connection_task.abort();
        let (written, skipped) = outcome?;

        // Block info, the number index and the resume pointer go in last and
        // in one fsynced batch: a crash before this leaves a database with no
        // head, which the next run overwrites.
        let mut batch = StateDBWrite::prepare_write_batch(&archive)?;
        StateDBWrite::write_block_hash(&archive, &mut batch, meta.number, meta.hash)?;
        StateDBWrite::write_block_info(&archive, &mut batch, block_info)?;
        StateDBWrite::write_latest_block_hash(&archive, &mut batch, meta.hash)?;
        batch.set_sync(true);
        StateDBWrite::commit(&archive, batch)?;

        info!(
            "block {} written: {} storage versions ({} rows skipped)",
            meta.number, written, skipped
        );
        info!(
            "next: archive-init --db-path {:?} --end-block <N> --statediff-key block-hash \
             (Lens stateDiffs are addressed by block hash, not state root)",
            self.db_path
        );
        Ok(())
    }

    /// Lay out and open the archive, refusing a database whose head is not the
    /// block before the one being imported.
    fn open_archive(&self) -> Result<Arc<ArchiveRocksDBStorage>> {
        crate::archive_udt::prepare_archive_schema(
            &self.db_path,
            self.archive_schema,
            self.inverted_block_encoding,
        )?;
        let archive = Arc::new(ArchiveRocksDBStorage::open_for_bulk_load(
            &self.db_path,
            self.db_cache,
            false,
        ));
        archive
            .write_encoding_marker(leafage_evm_storage::inverted_block_encoding())
            .map_err(|e| anyhow::anyhow!("failed to write the encoding marker: {e}"))?;

        let latest = archive
            .read_latest_block_hash()
            .map_err(|e| anyhow::anyhow!("failed to read the latest block hash: {e}"))?;
        if latest != H256::ZERO {
            let head = archive
                .read_block_info(latest)
                .map_err(|e| anyhow::anyhow!("failed to read the head block: {e}"))?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "the archive's latest hash {latest:#x} has no block info; delete the \
                         directory and re-run"
                    )
                })?;
            let expected = head.header.number + 1;
            if expected != self.block {
                bail!(
                    "the archive already holds blocks up to {}; import block {} instead",
                    head.header.number,
                    expected
                );
            }
            info!(
                "resuming an archive whose head is block {}",
                head.header.number
            );
        }
        Ok(archive)
    }

    async fn fetch_miniblock(&self, client: &tokio_postgres::Client) -> Result<MiniblockMeta> {
        let number = self.block as i64;
        let row = client
            .query_opt(
                "SELECT hash, timestamp, base_fee_per_gas::text, COALESCE(gas_limit, 0)::text, \
                        logs_bloom \
                 FROM miniblocks WHERE number = $1",
                &[&number],
            )
            .await
            .context("query miniblocks")?
            .ok_or_else(|| anyhow::anyhow!("miniblock {} is not in this database", self.block))?;

        let hash_bytes: Vec<u8> = row.get(0);
        if hash_bytes.len() != 32 {
            bail!(
                "miniblock {} has a {}-byte hash",
                self.block,
                hash_bytes.len()
            );
        }
        let timestamp: i64 = row.get(1);
        let base_fee: String = row.get(2);
        let gas_limit: String = row.get(3);
        let logs_bloom: Option<Vec<u8>> = row.get(4);

        let parent_hash = if self.block == 0 {
            H256::ZERO
        } else {
            let parent: Option<Vec<u8>> = client
                .query_opt(
                    "SELECT hash FROM miniblocks WHERE number = $1",
                    &[&(number - 1)],
                )
                .await
                .context("query the parent miniblock")?
                .map(|row| row.get(0));
            parent
                .filter(|h| h.len() == 32)
                .map(|h| H256::from_slice(&h))
                .unwrap_or(H256::ZERO)
        };

        // The debank header's gas_used is the sum of its transactions.
        let gas_used: String = client
            .query_one(
                "SELECT COALESCE(SUM((execution_info->>'gas_used')::numeric), 0)::text \
                 FROM transactions WHERE miniblock_number = $1",
                &[&number],
            )
            .await
            .context("query the block's gas usage")?
            .get(0);

        Ok(MiniblockMeta {
            number: self.block,
            hash: H256::from_slice(&hash_bytes),
            parent_hash,
            timestamp: u64::try_from(timestamp).unwrap_or(0),
            base_fee: parse_decimal(&base_fee).context("parse base_fee_per_gas")?,
            gas_limit: gas_limit.parse().unwrap_or(0),
            gas_used: parse_decimal(&gas_used)
                .context("parse gas_used")?
                .try_into()
                .unwrap_or(u64::MAX),
            logs_bloom: logs_bloom.unwrap_or_default(),
        })
    }

    /// Build the `BlockInfo` the way the producer writes it into S3: the
    /// fields it can compute, everything else defaulted (`state_root` is zero
    /// on every Era debank header).
    fn build_block_info(meta: &MiniblockMeta) -> Result<BlockInfo> {
        if meta.logs_bloom.len() != 256 {
            warn!(
                "block {} has a {}-byte logs_bloom; writing an empty one",
                meta.number,
                meta.logs_bloom.len()
            );
        }
        let bloom = if meta.logs_bloom.len() == 256 {
            format!("0x{}", alloy::hex::encode(&meta.logs_bloom))
        } else {
            format!("0x{}", "00".repeat(256))
        };
        let json = serde_json::json!({
            "hash": meta.hash,
            "parentHash": meta.parent_hash,
            "sha3Uncles": EMPTY_UNCLES,
            // A 20-byte address, not a hash: `H256::ZERO` here is 32 bytes and
            // alloy rejects it with "invalid string length".
            "miner": ZERO_ADDRESS,
            "stateRoot": H256::ZERO,
            "transactionsRoot": H256::ZERO,
            "receiptsRoot": H256::ZERO,
            "logsBloom": bloom,
            "difficulty": "0x0",
            "number": format!("0x{:x}", meta.number),
            "gasLimit": format!("0x{:x}", meta.gas_limit),
            "gasUsed": format!("0x{:x}", meta.gas_used),
            "timestamp": format!("0x{:x}", meta.timestamp),
            "extraData": "0x",
            "mixHash": H256::ZERO,
            "nonce": "0x0000000000000000",
            "baseFeePerGas": format!("{:#x}", meta.base_fee),
        });
        serde_json::from_value(json).context("build the block info")
    }

    /// Stream the miniblock's writes into the archive, committing in batches.
    /// Returns `(versions written, rows skipped)`.
    async fn copy_state(
        &self,
        archive: &Arc<ArchiveRocksDBStorage>,
        client: &mut tokio_postgres::Client,
        meta: &MiniblockMeta,
    ) -> Result<(u64, u64)> {
        let started = Instant::now();
        let tx = client
            .transaction()
            .await
            .context("start a Postgres transaction")?;
        // One worker and a physical-order scan: an append-only table then
        // yields rows in the order the node inserted them, so the last write
        // of a slot arrives last and wins, as on the live producer path.
        tx.batch_execute(
            "SET LOCAL max_parallel_workers_per_gather = 0; SET LOCAL enable_seqscan = off;",
        )
        .await
        .context("set the scan options")?;
        tx.batch_execute(&format!(
            "DECLARE state CURSOR FOR \
             SELECT address, key, value FROM storage_logs \
             WHERE miniblock_number = {} AND address IS NOT NULL AND key IS NOT NULL",
            meta.number
        ))
        .await
        .context("declare the storage_logs cursor")?;

        let mut batch = StateDBWrite::prepare_write_batch(archive)?;
        let mut buffered = 0usize;
        let mut written = 0u64;
        let mut skipped = 0u64;
        let mut last_report = Instant::now();

        loop {
            let rows = tx
                .query(&format!("FETCH {} FROM state", self.fetch_rows), &[])
                .await
                .context("fetch storage_logs rows")?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                let address: Vec<u8> = row.get(0);
                let key: Vec<u8> = row.get(1);
                let value: Vec<u8> = row.get(2);
                if address.len() != 20 || key.len() != 32 || value.len() > 32 {
                    skipped += 1;
                    continue;
                }
                let account = H256::from_slice(keccak256(&address).as_slice());
                let slot = H256::from_slice(keccak256(&key).as_slice());
                StateDBWrite::write_storage(
                    archive,
                    &mut batch,
                    account,
                    slot,
                    meta.number,
                    U256::from_be_slice(&value),
                )?;
                buffered += 1;
                written += 1;
                if buffered >= self.batch_entries {
                    StateDBWrite::commit(archive, batch)?;
                    batch = StateDBWrite::prepare_write_batch(archive)?;
                    buffered = 0;
                }
            }
            if last_report.elapsed() >= Duration::from_secs(10) {
                let elapsed = started.elapsed().as_secs_f64();
                info!(
                    "block {}: {} versions written ({:.0} rows/s, {} skipped)",
                    meta.number,
                    written,
                    written as f64 / elapsed.max(f64::EPSILON),
                    skipped
                );
                last_report = Instant::now();
            }
        }

        // New bytecode, keyed by hash, in the same batch as the tail of the
        // state so the block lands with its codes.
        let deps = tx
            .query(
                "SELECT bytecode_hash, bytecode FROM factory_deps WHERE miniblock_number = $1",
                &[&(meta.number as i64)],
            )
            .await
            .context("query factory_deps")?;
        let mut codes = 0usize;
        for row in deps {
            let hash: Vec<u8> = row.get(0);
            let code: Vec<u8> = row.get(1);
            if hash.len() != 32 {
                continue;
            }
            StateDBWrite::write_code(
                archive,
                &mut batch,
                H256::from_slice(&hash),
                Bytes::from(code),
            )?;
            codes += 1;
        }
        StateDBWrite::commit(archive, batch)?;
        tx.commit()
            .await
            .context("close the Postgres transaction")?;

        info!(
            "block {}: {} storage versions, {} factory deps in {:.1}s",
            meta.number,
            written,
            codes,
            started.elapsed().as_secs_f64()
        );
        Ok((written, skipped))
    }
}

/// Parse a Postgres `numeric` (rendered as decimal text) into a `U256`.
fn parse_decimal(text: &str) -> Result<U256> {
    let trimmed = text.trim();
    if let Some(hex) = trimmed.strip_prefix("0x") {
        return U256::from_str_radix(hex, 16)
            .with_context(|| format!("invalid hexadecimal number {trimmed:?}"));
    }
    // `numeric` may carry a fractional part ("0.000"); the header wants an
    // integer quantity.
    let integer = trimmed.split('.').next().unwrap_or("0");
    U256::from_str_radix(integer, 10).with_context(|| format!("invalid decimal number {trimmed:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_postgres_numeric_text() {
        assert_eq!(parse_decimal("0").unwrap(), 0u64);
        assert_eq!(parse_decimal("276356").unwrap(), 276356u64);
        assert_eq!(parse_decimal("0.000").unwrap(), 0u64);
        assert_eq!(parse_decimal("0xff").unwrap(), 255u64);
        assert!(parse_decimal("x").is_err());
    }

    /// The header JSON must round-trip through alloy's `Block`; a field of the
    /// wrong width (a 32-byte hash in the 20-byte `miner`) fails here rather
    /// than after the state copy has run.
    #[test]
    fn builds_a_block_info_alloy_accepts() {
        let meta = MiniblockMeta {
            number: 0,
            hash: H256::repeat_byte(0x11),
            parent_hash: H256::ZERO,
            timestamp: 0,
            base_fee: U256::ZERO,
            gas_limit: 0,
            gas_used: 0,
            logs_bloom: vec![0u8; 256],
        };
        let info = Command::build_block_info(&meta).unwrap();
        assert_eq!(info.header.hash, meta.hash);
        assert_eq!(info.header.inner.number, 0);
        assert_eq!(info.header.inner.parent_hash, H256::ZERO);
        assert_eq!(info.header.inner.beneficiary.as_slice(), &[0u8; 20]);
        assert_eq!(info.header.inner.timestamp, 0);
        assert_eq!(info.header.inner.base_fee_per_gas, Some(0));

        // A short bloom falls back to empty instead of failing.
        let mut short = meta;
        short.logs_bloom.clear();
        assert!(Command::build_block_info(&short).is_ok());
    }
}
