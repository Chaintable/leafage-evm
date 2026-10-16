use alloy::primitives::{Address, B256, U256};
use leafage_evm_storage::RocksDbReadProfile;
use revm::{bytecode::Bytecode, state::AccountInfo, DatabaseRef};
use serde::Serialize;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    rc::Rc,
    time::Instant,
};

// Per kind, per layer, per active call. Log only counts, never the keys. The
// bound also applies to adversarial calls and parallel workers independently.
const MAX_UNIQUE_KEYS: usize = 8192;

thread_local! {
    static CURRENT_READS: RefCell<Option<Rc<RefCell<ReadStats>>>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum Key {
    Account(Address),
    Code(B256),
    Storage(Address, U256),
    BlockHash(u64),
}

#[derive(Default)]
struct Metric {
    summary: MetricSummary,
    keys: HashSet<Key>,
}

#[derive(Clone, Default, Serialize)]
struct MetricSummary {
    count: u64,
    errors: u64,
    total_ms: f64,
    max_ms: f64,
    unique_keys: usize,
    unique_keys_truncated: bool,
    // Sum of successful code_by_hash return sizes at this boundary. Cached
    // returns count too; this is NOT the number of bytes read from the disk.
    returned_code_bytes: u64,
}

impl Metric {
    fn record(&mut self, key: Key, elapsed_ms: f64, error: bool, code_bytes: u64) {
        let s = &mut self.summary;
        s.count += 1;
        s.errors += u64::from(error);
        s.total_ms += elapsed_ms;
        s.max_ms = s.max_ms.max(elapsed_ms);
        s.returned_code_bytes += code_bytes;
        if self.keys.len() < MAX_UNIQUE_KEYS {
            self.keys.insert(key);
        } else if !self.keys.contains(&key) {
            s.unique_keys_truncated = true;
        }
        s.unique_keys = self.keys.len();
    }
}

#[derive(Default)]
struct Reads {
    account: Metric,
    storage: Metric,
    code: Metric,
    block_hash: Metric,
}

#[derive(Default, Serialize)]
struct ReadsSummary {
    account: MetricSummary,
    storage: MetricSummary,
    code: MetricSummary,
    block_hash: MetricSummary,
}

impl Reads {
    fn get_mut(&mut self, key: Key) -> &mut Metric {
        match key {
            Key::Account(_) => &mut self.account,
            Key::Code(_) => &mut self.code,
            Key::Storage(..) => &mut self.storage,
            Key::BlockHash(_) => &mut self.block_hash,
        }
    }

    fn count(&self) -> u64 {
        self.account.summary.count
            + self.storage.summary.count
            + self.code.summary.count
            + self.block_hash.summary.count
    }

    fn snapshot(&self) -> ReadsSummary {
        ReadsSummary {
            account: self.account.summary.clone(),
            storage: self.storage.summary.clone(),
            code: self.code.summary.clone(),
            block_hash: self.block_hash.summary.clone(),
        }
    }
}

#[derive(Default)]
struct ReadStats {
    request_cache: Reads,
    backing_state: Reads,
    request_cache_hits: u64,
    request_cache_misses: u64,
}

#[derive(Default, Serialize)]
pub(super) struct ReadSummary {
    // These are DatabaseRef accesses, NOT opcode counts. Backing-state timings
    // include layered/Moka cache lookups, key hashing and code decoding. They
    // are nested inside request-cache timings; do not sum the two layers.
    request_cache: ReadsSummary,
    backing_state: ReadsSummary,
    request_cache_hits: u64,
    request_cache_misses: u64,
    // Thread-local RocksDB counters exclude updater/other request threads.
    // Some timers overlap each other and backing_state.total_ms.
    rocksdb: BTreeMap<&'static str, u64>,
}

impl ReadSummary {
    pub(super) fn backing_total_ms(&self) -> f64 {
        let b = &self.backing_state;
        b.account.total_ms + b.storage.total_ms + b.code.total_ms + b.block_hash.total_ms
    }
}

pub(super) struct ReadScope {
    stats: Rc<RefCell<ReadStats>>,
    previous: Option<Rc<RefCell<ReadStats>>>,
    rocksdb: Option<RocksDbReadProfile>,
}

impl ReadScope {
    pub(super) fn new() -> Self {
        let stats = Rc::new(RefCell::new(ReadStats::default()));
        let previous = CURRENT_READS.replace(Some(stats.clone()));
        Self {
            stats,
            previous,
            rocksdb: RocksDbReadProfile::start(),
        }
    }

    pub(super) fn snapshot(&self) -> ReadSummary {
        let s = self.stats.borrow();
        ReadSummary {
            request_cache: s.request_cache.snapshot(),
            backing_state: s.backing_state.snapshot(),
            request_cache_hits: s.request_cache_hits,
            request_cache_misses: s.request_cache_misses,
            rocksdb: self
                .rocksdb
                .as_ref()
                .map(|r| r.snapshot())
                .unwrap_or_default(),
        }
    }
}

impl Drop for ReadScope {
    fn drop(&mut self) {
        CURRENT_READS.set(self.previous.take());
    }
}

/// Stores no per-thread profiling state: CacheDB clones remain Send and each
/// pooled worker constructs its own ReadScope only after it starts executing.
#[derive(Clone)]
pub(crate) struct ObservedDb<DB> {
    pub(crate) db: DB,
    backing: bool,
}

impl<DB> ObservedDb<DB> {
    pub(crate) fn backing(db: DB) -> Self {
        Self { db, backing: true }
    }
    pub(crate) fn cached(db: DB) -> Self {
        Self { db, backing: false }
    }
}

impl<DB> std::fmt::Debug for ObservedDb<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservedDb")
            .field("backing", &self.backing)
            .finish_non_exhaustive()
    }
}

impl<DB: DatabaseRef> ObservedDb<DB> {
    fn read<T>(
        &self,
        key: Key,
        read: impl FnOnce(&DB) -> Result<T, DB::Error>,
        code_bytes: impl FnOnce(&T) -> u64,
    ) -> Result<T, DB::Error> {
        let Some(stats) = CURRENT_READS.with(|s| s.borrow().clone()) else {
            return read(&self.db);
        };
        let before = stats.borrow().backing_state.count();
        let started = Instant::now();
        // A request-cache miss reenters the backing wrapper using this same
        // RefCell. Never hold a borrow while calling the wrapped database.
        let result = read(&self.db);
        let elapsed_ms = super::ms(started.elapsed());
        let bytes = result.as_ref().map(code_bytes).unwrap_or_default();
        let mut s = stats.borrow_mut();
        if self.backing {
            s.backing_state
                .get_mut(key)
                .record(key, elapsed_ms, result.is_err(), bytes);
        } else {
            s.request_cache
                .get_mut(key)
                .record(key, elapsed_ms, result.is_err(), bytes);
            // A storage miss may load both an account AND a slot. Compare all
            // backing reads, not just same-kind counts, to count one cache miss.
            if s.backing_state.count() == before {
                if result.is_ok() {
                    s.request_cache_hits += 1;
                }
            } else {
                s.request_cache_misses += 1;
            }
        }
        result
    }
}

impl<DB: DatabaseRef> DatabaseRef for ObservedDb<DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.read(Key::Account(address), |db| db.basic_ref(address), |_| 0)
    }
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.read(
            Key::Storage(address, index),
            |db| db.storage_ref(address, index),
            |_| 0,
        )
    }
    fn code_by_hash_ref(&self, hash: B256) -> Result<Bytecode, Self::Error> {
        self.read(
            Key::Code(hash),
            |db| db.code_by_hash_ref(hash),
            |code| code.original_bytes().len() as u64,
        )
    }
    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.read(
            Key::BlockHash(number),
            |db| db.block_hash_ref(number),
            |_| 0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_impl::utils::RequestCacheDB;
    use revm::database::{CacheDB, InMemoryDB};

    #[test]
    fn nested_misses_cache_hits_and_cross_call_cache_reuse_are_distinct() {
        let address = Address::repeat_byte(1);
        let mut backing = InMemoryDB::default();
        backing.insert_account_info(
            address,
            AccountInfo {
                nonce: 1,
                ..Default::default()
            },
        );
        backing
            .insert_account_storage(address, U256::ZERO, U256::from(42))
            .unwrap();
        let db = ObservedDb::cached(RequestCacheDB::new(CacheDB::new(ObservedDb::backing(
            backing,
        ))));
        {
            let scope = ReadScope::new();
            for _ in 0..2 {
                assert_eq!(db.storage_ref(address, U256::ZERO).unwrap(), U256::from(42));
            }
            assert_eq!(db.basic_ref(address).unwrap().unwrap().nonce, 1);
            let s = scope.snapshot();
            assert_eq!(s.request_cache.storage.count, 2);
            assert_eq!(s.request_cache.storage.unique_keys, 1);
            assert_eq!(s.backing_state.storage.count, 1);
            assert_eq!(s.backing_state.account.count, 1);
            assert_eq!(s.request_cache_misses, 1);
            assert_eq!(s.request_cache_hits, 2);
        }
        let next_call = ReadScope::new();
        db.storage_ref(address, U256::ZERO).unwrap();
        let s = next_call.snapshot();
        assert_eq!(s.request_cache_hits, 1);
        assert_eq!(s.backing_state.storage.count, 0);
    }

    #[test]
    fn code_volume_unique_key_bound_and_panic_cleanup() {
        let mut backing = InMemoryDB::default();
        let code = Bytecode::new_raw(vec![0x60, 0x00, 0x00].into());
        let hash = code.hash_slow();
        backing.insert_account_info(
            Address::ZERO,
            AccountInfo {
                code: Some(code),
                code_hash: hash,
                ..Default::default()
            },
        );
        let scope = ReadScope::new();
        let db = ObservedDb::backing(backing);
        for _ in 0..2 {
            assert_eq!(db.code_by_hash_ref(hash).unwrap().original_bytes().len(), 3);
        }
        let s = scope.snapshot();
        assert_eq!(s.backing_state.code.returned_code_bytes, 6);
        assert_eq!(s.backing_state.code.unique_keys, 1);
        let mut metric = Metric::default();
        for i in 0..=MAX_UNIQUE_KEYS {
            metric.record(Key::BlockHash(i as u64), 1.0, false, 0);
        }
        assert_eq!(metric.summary.count, (MAX_UNIQUE_KEYS + 1) as u64);
        assert_eq!(metric.keys.len(), MAX_UNIQUE_KEYS);
        assert!(metric.summary.unique_keys_truncated);
        drop(scope);
        let _ = std::panic::catch_unwind(|| {
            let _scope = ReadScope::new();
            panic!("test unwind");
        });
        assert!(CURRENT_READS.with(|s| s.borrow().is_none()));
        assert!(RocksDbReadProfile::start().is_some());
    }

    #[test]
    fn pooled_threads_do_not_share_read_counters() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let joins: Vec<_> = [3, 7]
            .into_iter()
            .map(|count| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let scope = ReadScope::new();
                    let db = ObservedDb::backing(InMemoryDB::default());
                    barrier.wait();
                    for _ in 0..count {
                        db.basic_ref(Address::ZERO).unwrap();
                    }
                    assert_eq!(scope.snapshot().backing_state.account.count, count);
                })
            })
            .collect();
        for join in joins {
            join.join().unwrap();
        }
        assert!(CURRENT_READS.with(|s| s.borrow().is_none()));
    }
}
