//! `leafage-evm archive-clear-deleted` — zero the storage an archive kept for
//! deleted accounts.
//!
//! A block diff lists a destroyed contract among its deleted accounts but not
//! its storage, and archives written before deletions zeroed that storage
//! (or built by `archive-init`, which cannot) still return the old slots
//! after the deletion: wrong once the address is created again, as `CREATE2`
//! does with the same code. This writes, for every deletion at block `d`, a
//! zero version at `d` of each slot that was non-zero then, unless block `d`
//! wrote the slot itself.
//!
//! It runs in two steps. The scan opens the archive read-only, so it can run
//! next to a process using it, and writes the slots to zero to `--plan`, one
//! `address slot block recreated` line each (hashed keys, as stored;
//! `recreated` when the account exists again after the deletion). `--apply`
//! then writes the plan into the archive, which it needs to itself.

use alloy::primitives::{hex, U256};
use anyhow::{anyhow, bail, Context, Result};
use leafage_evm_storage::rocksdb::{DBRawIteratorWithThreadMode, Options, ReadOptions, DB};
use leafage_evm_storage::{ArchiveRocksDBStorage, StateDBProvider, StateDBRead, StateDBWrite};
use leafage_evm_types::{BlockId, BlockNumberOrTag, H256};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::info;

/// Account column family: `address(32) || version(32)`.
const ACCOUNT_CF: &str = "4";
/// Storage column family: `address(32) || slot(32) || version(32)`.
const STORAGE_CF: &str = "5";
/// Slots zeroed in one write batch by `--apply`.
const APPLY_BATCH: usize = 100_000;

#[derive(Debug, clap::Parser)]
pub struct Command {
    /// Path to the archive RocksDB.
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// The archive's version tails are descending (inverted).
    #[arg(long, default_value = "false")]
    inverted_block_encoding: bool,

    /// The plan: written by the scan, read by `--apply`.
    #[arg(long, value_name = "PATH")]
    plan: PathBuf,

    /// Write the plan into the archive instead of scanning.
    #[arg(long, default_value = "false")]
    apply: bool,

    /// Scan threads.
    #[arg(long, default_value_t = 16)]
    threads: usize,

    /// RocksDB max open files of the read-only scan.
    #[arg(long, default_value_t = 4096)]
    max_open_files: i32,

    /// Block cache of `--apply`, MB.
    #[arg(long, default_value = "1024")]
    cache_size: usize,

    #[arg(long, default_value = "false")]
    archive_zstd_compression: bool,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        if !self.db_path.join("CURRENT").exists() {
            bail!("no RocksDB database found at {:?}", self.db_path);
        }
        if self.apply {
            self.apply()
        } else {
            self.scan()
        }
    }

    fn scan(&self) -> Result<()> {
        let mut opts = Options::default();
        opts.set_max_open_files(self.max_open_files);
        let db =
            DB::open_cf_for_read_only(&opts, &self.db_path, ["1", "2", "3", "4", "5", "6"], false)
                .map_err(|e| anyhow!("open read-only: {e}"))?;
        let plan = Mutex::new(BufWriter::new(
            File::create(&self.plan).with_context(|| format!("creating {:?}", self.plan))?,
        ));
        let stats = Stats::default();
        let next_shard = AtomicUsize::new(0);
        let started = Instant::now();
        let scanner = Scanner {
            db: &db,
            inverted: self.inverted_block_encoding,
            plan: &plan,
            stats: &stats,
        };
        std::thread::scope(|scope| -> Result<()> {
            let workers: Vec<_> = (0..self.threads.max(1))
                .map(|_| {
                    scope.spawn(|| -> Result<()> {
                        loop {
                            let shard = next_shard.fetch_add(1, Ordering::Relaxed);
                            if shard > u8::MAX as usize {
                                return Ok(());
                            }
                            scanner.shard(shard as u8)?;
                            stats.shards.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            let mut last = Instant::now();
            while !workers.iter().all(|w| w.is_finished()) {
                std::thread::sleep(Duration::from_millis(200));
                if last.elapsed() >= Duration::from_secs(60) {
                    last = Instant::now();
                    stats.log(started.elapsed());
                }
            }
            for worker in workers {
                worker
                    .join()
                    .map_err(|_| anyhow!("scan thread panicked"))??;
            }
            Ok(())
        })?;
        plan.into_inner().unwrap().flush()?;
        stats.log(started.elapsed());
        info!(target: "archive_clear_deleted", "plan written to {:?}", self.plan);
        Ok(())
    }

    fn apply(&self) -> Result<()> {
        leafage_evm_storage::set_inverted_block_encoding(self.inverted_block_encoding);
        let entries = read_plan(&self.plan)?;
        info!(target: "archive_clear_deleted", "{} slots to zero", entries.len());
        let db = Arc::new(ArchiveRocksDBStorage::open(
            &self.db_path,
            self.cache_size,
            false,
            self.archive_zstd_compression,
        ));
        for (index, chunk) in entries.chunks(APPLY_BATCH).enumerate() {
            let mut batch = db.prepare_write_batch()?;
            for &(address, slot, block) in chunk {
                db.write_storage(&mut batch, address, slot, block, U256::ZERO)?;
            }
            db.commit(batch)?;
            info!(
                target: "archive_clear_deleted",
                "zeroed {} of {} slots",
                (index * APPLY_BATCH + chunk.len()),
                entries.len()
            );
        }
        // Read every entry back as a zero of its block.
        for &(address, slot, block) in &entries {
            let state = db
                .db_at(BlockId::Number(BlockNumberOrTag::Number(block)))?
                .ok_or_else(|| anyhow!("no state at block {block}"))?;
            let read = state.read_storage_and_height(address, slot)?;
            if read != (U256::ZERO, Some(block)) {
                bail!("{address} {slot} at {block} reads {read:?} after the write");
            }
        }
        info!(target: "archive_clear_deleted", "zeroed {} slots and read them back", entries.len());
        Ok(())
    }
}

#[derive(Default)]
struct Stats {
    shards: AtomicU64,
    account_versions: AtomicU64,
    deletions: AtomicU64,
    /// Deleted accounts with any storage.
    with_storage: AtomicU64,
    /// Deleted accounts with a slot to zero.
    to_clear: AtomicU64,
    /// Of those, created again after the deletion.
    recreated: AtomicU64,
    slots: AtomicU64,
}

impl Stats {
    fn log(&self, elapsed: Duration) {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        info!(
            target: "archive_clear_deleted",
            shards = format!("{}/256", get(&self.shards)),
            account_versions = get(&self.account_versions),
            deletions = get(&self.deletions),
            deleted_with_storage = get(&self.with_storage),
            accounts_to_clear = get(&self.to_clear),
            recreated = get(&self.recreated),
            slots_to_zero = get(&self.slots),
            elapsed_s = elapsed.as_secs(),
            "scan"
        );
    }
}

struct Scanner<'a> {
    db: &'a DB,
    inverted: bool,
    plan: &'a Mutex<BufWriter<File>>,
    stats: &'a Stats,
}

impl Scanner<'_> {
    fn decode_version(&self, tail: &[u8]) -> u64 {
        let raw = u64::from_be_bytes(tail[24..32].try_into().expect("32-byte version tail"));
        if self.inverted {
            u64::MAX - raw
        } else {
            raw
        }
    }

    fn encode_version(&self, block: u64) -> [u8; 32] {
        let mut tail = [0u8; 32];
        let raw = if self.inverted {
            u64::MAX - block
        } else {
            block
        };
        tail[24..].copy_from_slice(&raw.to_be_bytes());
        tail
    }

    fn iterator(
        &self,
        cf: &str,
        bounds: Option<(u8, Option<u8>)>,
    ) -> DBRawIteratorWithThreadMode<'_, DB> {
        let mut read_options = ReadOptions::default();
        read_options.set_verify_checksums(false);
        read_options.set_total_order_seek(true);
        if let Some((lower, upper)) = bounds {
            read_options.set_readahead_size(16 * 1024 * 1024);
            read_options.set_iterate_lower_bound(vec![lower]);
            if let Some(upper) = upper {
                read_options.set_iterate_upper_bound(vec![upper]);
            }
        }
        self.db.raw_iterator_cf_opt(
            self.db.cf_handle(cf).expect("archive column family"),
            read_options,
        )
    }

    /// The accounts whose hashed address starts with `shard`.
    fn shard(&self, shard: u8) -> Result<()> {
        let mut accounts = self.iterator(ACCOUNT_CF, Some((shard, shard.checked_add(1))));
        let mut storage = self.iterator(STORAGE_CF, None);
        accounts.seek_to_first();
        let mut current: Option<Account> = None;
        let mut versions = 0u64;
        while let Some(key) = accounts.key() {
            let address: [u8; 32] = key[..32].try_into()?;
            let block = self.decode_version(&key[32..64]);
            let deleted = accounts.value().is_some_and(<[u8]>::is_empty);
            if current
                .as_ref()
                .is_some_and(|account| account.address != address)
            {
                self.account(&mut storage, current.take().expect("checked"))?;
            }
            let account = current.get_or_insert_with(|| Account {
                address,
                deletions: Vec::new(),
                last_live: None,
            });
            if deleted {
                account.deletions.push(block);
            } else {
                account.last_live = account.last_live.max(Some(block));
            }
            versions += 1;
            if versions.is_multiple_of(1_000_000) {
                self.stats
                    .account_versions
                    .fetch_add(1_000_000, Ordering::Relaxed);
            }
            accounts.next();
        }
        accounts.status()?;
        self.stats
            .account_versions
            .fetch_add(versions % 1_000_000, Ordering::Relaxed);
        if let Some(account) = current {
            self.account(&mut storage, account)?;
        }
        Ok(())
    }

    /// Plan the slots of a deleted `account` to zero.
    fn account(
        &self,
        storage: &mut DBRawIteratorWithThreadMode<'_, DB>,
        account: Account,
    ) -> Result<()> {
        if account.deletions.is_empty() {
            return Ok(());
        }
        self.stats
            .deletions
            .fetch_add(account.deletions.len() as u64, Ordering::Relaxed);
        let mut lines = Vec::new();
        let mut recreated = false;
        let mut any_storage = false;
        let mut from = Some(U256::ZERO);
        while let Some(start) = from {
            // The first slot at or after `start`: an all-zero version tail
            // sorts first under both encodings.
            let mut key = [0u8; 96];
            key[..32].copy_from_slice(&account.address);
            key[32..64].copy_from_slice(&start.to_be_bytes::<32>());
            storage.seek(key);
            let slot: [u8; 32] = match storage.key() {
                Some(found) if found[..32] == account.address => found[32..64].try_into()?,
                _ => break,
            };
            any_storage = true;
            for &deleted in &account.deletions {
                // The slot's value as of the deletion block.
                key[32..64].copy_from_slice(&slot);
                key[64..].copy_from_slice(&self.encode_version(deleted));
                if self.inverted {
                    storage.seek(key);
                } else {
                    storage.seek_for_prev(key);
                }
                let Some(found) = storage.key() else { continue };
                if found[..64] != key[..64] {
                    continue;
                }
                // Written by the deleting block itself: that write stands.
                if self.decode_version(&found[64..96]) == deleted {
                    continue;
                }
                if storage
                    .value()
                    .is_some_and(|value| value.iter().any(|b| *b != 0))
                {
                    let again = account.last_live.is_some_and(|live| live > deleted);
                    recreated |= again;
                    lines.push(format!(
                        "0x{} 0x{} {deleted} {again}\n",
                        hex::encode(account.address),
                        hex::encode(slot)
                    ));
                }
            }
            from = U256::from_be_bytes(slot).checked_add(U256::from(1));
        }
        storage.status()?;
        if any_storage {
            self.stats.with_storage.fetch_add(1, Ordering::Relaxed);
        }
        if lines.is_empty() {
            return Ok(());
        }
        self.stats.to_clear.fetch_add(1, Ordering::Relaxed);
        self.stats
            .recreated
            .fetch_add(recreated as u64, Ordering::Relaxed);
        self.stats
            .slots
            .fetch_add(lines.len() as u64, Ordering::Relaxed);
        let mut plan = self.plan.lock().unwrap();
        for line in lines {
            plan.write_all(line.as_bytes())?;
        }
        Ok(())
    }
}

struct Account {
    address: [u8; 32],
    deletions: Vec<u64>,
    /// The newest block the account exists at.
    last_live: Option<u64>,
}

fn read_plan(path: &PathBuf) -> Result<Vec<(H256, H256, u64)>> {
    let file = File::open(path).with_context(|| format!("opening {path:?}"))?;
    let mut entries = Vec::new();
    for (number, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        let parse = || -> Result<(H256, H256, u64)> {
            let mut fields = line.split_whitespace();
            let mut next = || fields.next().ok_or_else(|| anyhow!("too few fields"));
            Ok((next()?.parse()?, next()?.parse()?, next()?.parse()?))
        };
        entries.push(parse().with_context(|| format!("{path:?} line {}", number + 1))?);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_storage::{EvmStorageWrite, StateDBWrapper};
    use leafage_evm_types::{
        AccountStorageDiff, Block, BlockInfo, BlockStorageDiff, Header, IndexValuePair, NewAccount,
        RawHeader, KECCAK256_EMPTY,
    };

    fn block_info(number: u64) -> BlockInfo {
        BlockInfo::new(Block {
            header: Header {
                hash: H256::with_last_byte(number as u8),
                inner: RawHeader {
                    number,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn diff(address: H256, slots: &[(H256, u64)], account: bool) -> BlockStorageDiff {
        BlockStorageDiff {
            new_accounts: account
                .then(|| NewAccount {
                    address,
                    balance: U256::ZERO,
                    nonce: 1,
                    code_hash: KECCAK256_EMPTY.0.into(),
                })
                .into_iter()
                .collect(),
            storage_diffs: vec![AccountStorageDiff {
                address,
                diffs: slots
                    .iter()
                    .map(|(index, value)| IndexValuePair {
                        index: *index,
                        value: U256::from(*value),
                    })
                    .collect(),
            }],
            ..Default::default()
        }
    }

    /// An archive written before deletions zeroed storage: the scan plans the
    /// slots, the apply zeroes them, and the deleting block's own writes stand.
    fn run(inverted: bool) {
        leafage_evm_storage::set_inverted_block_encoding(inverted);
        let dir = std::env::temp_dir().join(format!(
            "leafage-clear-deleted-{inverted}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("db");
        let (addr, next) = (H256::repeat_byte(0xab), H256::repeat_byte(0xac));
        let (a, b) = (H256::repeat_byte(0x01), H256::repeat_byte(0x02));
        {
            let db = Arc::new(ArchiveRocksDBStorage::open(&db_path, 64, false, false));
            let commit = |n: u64, diff: BlockStorageDiff, delete: bool| {
                let latest = db
                    .db_at(BlockId::Number(BlockNumberOrTag::Latest))
                    .unwrap()
                    .unwrap();
                StateDBWrapper(latest)
                    .update_block(block_info(n), diff)
                    .unwrap();
                if delete {
                    // The deletion as older archives hold it: no storage zeroed.
                    let mut batch = db.prepare_write_batch().unwrap();
                    db.write_account(&mut batch, addr, n, None).unwrap();
                    db.commit(batch).unwrap();
                }
            };
            let mut first = diff(addr, &[(a, 7), (b, 9)], true);
            first
                .storage_diffs
                .extend(diff(next, &[(b, 1)], false).storage_diffs);
            commit(1, first, false);
            commit(2, BlockStorageDiff::default(), true);
            commit(3, diff(addr, &[(b, 5)], true), false);
            commit(4, diff(addr, &[(a, 11)], false), true);
        }
        let mut command = Command {
            db_path: db_path.clone(),
            inverted_block_encoding: inverted,
            plan: dir.join("plan"),
            apply: false,
            threads: 2,
            max_open_files: 64,
            cache_size: 64,
            archive_zstd_compression: false,
        };
        command.scan().unwrap();
        let mut plan = read_plan(&command.plan).unwrap();
        plan.sort();
        assert_eq!(
            plan,
            [(addr, a, 2), (addr, b, 2), (addr, b, 4)],
            "inverted={inverted}"
        );
        let lines = std::fs::read_to_string(&command.plan).unwrap();
        assert_eq!(
            lines.matches(" true\n").count(),
            2,
            "recreated after 2 only"
        );

        command.apply = true;
        command.apply().unwrap();
        let db = Arc::new(ArchiveRocksDBStorage::open(&db_path, 64, false, false));
        let read = |n: u64, address: H256, slot: H256| {
            db.db_at(BlockId::Number(BlockNumberOrTag::Number(n)))
                .unwrap()
                .unwrap()
                .read_storage(address, slot)
                .unwrap()
                .to::<u64>()
        };
        assert_eq!([read(1, addr, a), read(1, addr, b)], [7, 9]);
        assert_eq!([read(2, addr, a), read(2, addr, b)], [0, 0]);
        assert_eq!([read(3, addr, a), read(3, addr, b)], [0, 5]);
        assert_eq!([read(4, addr, a), read(4, addr, b)], [11, 0]);
        assert_eq!(read(4, next, b), 1);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clears_deleted_storage() {
        // One test: the archive backend keeps its DB in a process global.
        run(true);
        run(false);
        leafage_evm_storage::set_inverted_block_encoding(false);
    }
}
