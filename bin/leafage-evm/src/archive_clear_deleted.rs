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
//! A contract destroyed and created again within one block leaves no deletion
//! at all, only a new account version. With `--bsc-mainnet` such a block also
//! counts as a deletion before Cancun (EIP-6780), told by the code hash
//! changing or the nonce going down, which nothing else does to a contract
//! then; the system contracts, upgraded in place, aside.
//!
//! It runs in two steps. The scan opens the archive read-only, so it can run
//! next to a process using it, and writes the slots to zero to `--plan`, one
//! `address slot block recreated` line each (hashed keys, as stored;
//! `recreated` when the account exists at or after the deletion). `--apply`
//! then writes the plan into the archive, which it needs to itself.

use alloy::primitives::{hex, keccak256, U256};
use alloy_rlp::Decodable;
use anyhow::{anyhow, bail, Context, Result};
use leafage_evm_chains::bsc::mainnet::{
    BSC_MAINNET_CANCUN_BLOCK, BSC_MAINNET_UPGRADABLE_CONTRACTS,
};
use leafage_evm_storage::rocksdb::{DBRawIteratorWithThreadMode, Options, ReadOptions, DB};
use leafage_evm_storage::{ArchiveRocksDBStorage, StateDBProvider, StateDBRead, StateDBWrite};
use leafage_evm_types::{BlockId, BlockNumberOrTag, SlimAccount, H256, KECCAK256_EMPTY};
use std::collections::HashSet;
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

    /// The archive is BSC mainnet's: also plan the zeros for contracts
    /// destroyed and created again within one block before Cancun.
    #[arg(long, default_value = "false")]
    bsc_mainnet: bool,

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
        let recreations = self.bsc_mainnet.then(Recreations::bsc_mainnet);
        self.scan_with(recreations.as_ref())
    }

    fn scan_with(&self, recreations: Option<&Recreations>) -> Result<()> {
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
            recreations,
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
    /// Contracts destroyed and created again within one block.
    recreations: AtomicU64,
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
            recreations = get(&self.recreations),
            deleted_with_storage = get(&self.with_storage),
            accounts_to_clear = get(&self.to_clear),
            recreated = get(&self.recreated),
            slots_to_zero = get(&self.slots),
            elapsed_s = elapsed.as_secs(),
            "scan"
        );
    }
}

/// Tells a contract destroyed and created again within one block, which
/// leaves no deletion, from two consecutive versions of the account.
struct Recreations {
    /// From this block on contracts are not destroyed (EIP-6780).
    before: u64,
    /// Hashed addresses of the contracts whose code changes in place.
    upgradable: HashSet<[u8; 32]>,
}

impl Recreations {
    fn bsc_mainnet() -> Self {
        Self {
            before: BSC_MAINNET_CANCUN_BLOCK,
            upgradable: BSC_MAINNET_UPGRADABLE_CONTRACTS
                .iter()
                .map(|address| keccak256(address).0)
                .collect(),
        }
    }

    /// Whether `address` went from `older` to `newer` (block, code hash and
    /// nonce) by being destroyed and created again in `newer`'s block.
    fn between(&self, address: &[u8; 32], older: Live, newer: (u64, Live)) -> bool {
        let (Some((code_before, nonce_before)), (block, Some((code, nonce)))) = (older, newer)
        else {
            return false;
        };
        block < self.before
            && !self.upgradable.contains(address)
            && (code != code_before || nonce < nonce_before)
    }
}

/// The code hash and nonce of an account version; `None` if deleted.
type Live = Option<(H256, u64)>;

fn decode_live(value: &[u8]) -> Result<Live> {
    if value.is_empty() {
        return Ok(None);
    }
    let account = SlimAccount::decode(&mut &value[..]).map_err(|e| anyhow!("account: {e}"))?;
    let code_hash = if account.code_hash.is_zero() {
        KECCAK256_EMPTY.0.into()
    } else {
        account.code_hash
    };
    Ok(Some((code_hash, account.nonce)))
}

struct Scanner<'a> {
    db: &'a DB,
    inverted: bool,
    recreations: Option<&'a Recreations>,
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
                recreations: Vec::new(),
                last_live: None,
                previous: None,
            });
            if deleted {
                account.deletions.push(block);
            } else {
                account.last_live = account.last_live.max(Some(block));
            }
            if let Some(recreations) = self.recreations {
                let live = decode_live(accounts.value().unwrap_or_default())?;
                if let Some(seen) = account.previous.replace((block, live)) {
                    // Versions come oldest first, or newest first if inverted.
                    let (older, newer) = if self.inverted {
                        (live, seen)
                    } else {
                        (seen.1, (block, live))
                    };
                    if recreations.between(&address, older, newer) {
                        account.recreations.push(newer.0);
                    }
                }
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
        self.stats
            .deletions
            .fetch_add(account.deletions.len() as u64, Ordering::Relaxed);
        self.stats
            .recreations
            .fetch_add(account.recreations.len() as u64, Ordering::Relaxed);
        let deletions: Vec<u64> = account
            .deletions
            .iter()
            .chain(&account.recreations)
            .copied()
            .collect();
        if deletions.is_empty() {
            return Ok(());
        }
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
            for &deleted in &deletions {
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
                    let again = account.last_live.is_some_and(|live| live >= deleted);
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
    /// Blocks that destroyed and created it again, see [`Recreations`].
    recreations: Vec<u64>,
    /// The newest block the account exists at.
    last_live: Option<u64>,
    /// The version scanned last, with [`Recreations`].
    previous: Option<(u64, Live)>,
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

    /// A contract with code `code`.
    fn contract(address: H256, code: u8, nonce: u64) -> NewAccount {
        NewAccount {
            address,
            balance: U256::ZERO,
            nonce,
            code_hash: H256::repeat_byte(code),
        }
    }

    /// An archive written before deletions zeroed storage: the scan plans the
    /// slots, the apply zeroes them, and the deleting block's own writes stand.
    /// `meta` is destroyed and created again within blocks 2 (other code) and
    /// 4 (lower nonce), leaving no deletion; `system` is upgraded in place at
    /// 2, and `meta` changes code again at 5, past the rules' Cancun.
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
        let (meta, system) = (H256::repeat_byte(0xcd), H256::repeat_byte(0xee));
        let (a, b) = (H256::repeat_byte(0x01), H256::repeat_byte(0x02));
        let with = |mut block: BlockStorageDiff,
                    accounts: &[NewAccount],
                    slots: &[(H256, &[(H256, u64)])]| {
            block.new_accounts.extend_from_slice(accounts);
            for (address, slots) in slots {
                block
                    .storage_diffs
                    .extend(diff(*address, slots, false).storage_diffs);
            }
            block
        };
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
            let first = with(
                diff(addr, &[(a, 7), (b, 9)], true),
                &[contract(meta, 1, 1), contract(system, 1, 1)],
                &[
                    (next, &[(b, 1)]),
                    (meta, &[(a, 3), (b, 4)]),
                    (system, &[(a, 1)]),
                ],
            );
            commit(1, first, false);
            let second = with(
                BlockStorageDiff::default(),
                &[contract(meta, 2, 1), contract(system, 2, 1)],
                &[(meta, &[(b, 8)])],
            );
            commit(2, second, true);
            let third = with(diff(addr, &[(b, 5)], true), &[contract(meta, 2, 2)], &[]);
            commit(3, third, false);
            let fourth = with(diff(addr, &[(a, 11)], false), &[contract(meta, 2, 1)], &[]);
            commit(4, fourth, true);
            commit(
                5,
                with(Default::default(), &[contract(meta, 3, 1)], &[]),
                false,
            );
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
            bsc_mainnet: false,
        };
        let recreations = Recreations {
            before: 5,
            upgradable: HashSet::from([system.0]),
        };
        command.scan_with(Some(&recreations)).unwrap();
        let mut plan = read_plan(&command.plan).unwrap();
        plan.sort();
        assert_eq!(
            plan,
            [
                (addr, a, 2),
                (addr, b, 2),
                (addr, b, 4),
                (meta, a, 2),
                (meta, a, 4),
                (meta, b, 4),
            ],
            "inverted={inverted}"
        );
        let lines = std::fs::read_to_string(&command.plan).unwrap();
        assert_eq!(
            lines.matches(" true\n").count(),
            5,
            "all but addr's deletion at 4"
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
        assert_eq!([read(1, meta, a), read(1, meta, b)], [3, 4]);
        assert_eq!([read(2, meta, a), read(2, meta, b)], [0, 8]);
        assert_eq!([read(3, meta, a), read(3, meta, b)], [0, 8]);
        assert_eq!([read(4, meta, a), read(4, meta, b)], [0, 0]);
        assert_eq!(read(2, system, a), 1);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clears_deleted_storage() {
        let _guard = crate::ARCHIVE_TEST_LOCK.blocking_lock();
        run(true);
        run(false);
        leafage_evm_storage::set_inverted_block_encoding(false);
    }
}
