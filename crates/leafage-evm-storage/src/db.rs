use crate::db_impl::StorageError;
use crate::interface::{BlockContext, EvmStorageWrite, StateDB};
use crate::metrics::STORAGE_METRICS;
use auto_impl::auto_impl;
use leafage_evm_types::{
    AccountInfo, BlockId, BlockInfo, BlockStorageDiff, Bytecode, Bytes, NewAccount, H256, U256,
};
use moka::sync::Cache as MokaCache;
use std::fmt::Debug;
use std::sync::LazyLock;
use tracing::{debug, info, trace};

/// Default weighted capacity (MB) of the analyzed-bytecode cache.
const DEFAULT_ANALYZED_CODE_CACHE_MB: u64 = 128;

/// Approximate fixed cost of one cache entry (key, `Arc`s, cache metadata).
const ANALYZED_CODE_ENTRY_OVERHEAD: usize = 128;

/// Process-wide cache of analyzed bytecode, keyed by code hash.
///
/// [`Bytecode::new_raw`] walks the whole contract to build its jump table.
/// Without this cache every request that touches a contract repeats that
/// walk (a request-scoped `CacheDB` only dedupes within one request); on a
/// historical-read workload it was ~4% of node CPU. The key is the keccak
/// of the code, so an entry never goes stale and needs no invalidation, and
/// `Bytecode` is an `Arc`, so a hit costs a reference-count bump.
/// `ANALYZED_CODE_CACHE_MB=0` disables it.
static ANALYZED_CODE_CACHE: LazyLock<Option<MokaCache<H256, Bytecode>>> = LazyLock::new(|| {
    let mb = std::env::var("ANALYZED_CODE_CACHE_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_ANALYZED_CODE_CACHE_MB);
    if mb == 0 {
        info!(target = "storage", "analyzed bytecode cache disabled");
        return None;
    }
    info!(target = "storage", "analyzed bytecode cache enabled: {}MB", mb);
    Some(
        MokaCache::builder()
            .max_capacity(mb.saturating_mul(1024 * 1024))
            // Padded code bytes, the one-bit-per-byte jump table, and a rough
            // allowance for the key and per-entry bookkeeping. moka treats the
            // capacity as best-effort, so this is a budget, not a hard limit.
            .weigher(|_k: &H256, v: &Bytecode| {
                let len = v.bytes_slice().len();
                (len + len.div_ceil(8) + ANALYZED_CODE_ENTRY_OVERHEAD)
                    .try_into()
                    .unwrap_or(u32::MAX)
            })
            .build(),
    )
});

/// Analyze `code` and remember the result under `code_hash`.
fn analyze_and_cache(code_hash: H256, code: Bytes) -> Bytecode {
    let bytecode = Bytecode::new_raw(code.0.into());
    if let Some(cache) = ANALYZED_CODE_CACHE.as_ref() {
        cache.insert(code_hash, bytecode.clone());
    }
    bytecode
}

#[auto_impl(&, Box, Arc)]
pub trait BlockIterator: Send + Sync + 'static {
    /// block num -> block info
    fn block_info_iter(&self) -> impl Iterator<Item = Result<BlockInfo, StorageError>>;

    /// block num -> block hash
    fn block_hash_iter(&self) -> impl Iterator<Item = Result<(u64, H256), StorageError>>;
}

/// [`StateDBRead`] offers read-only access to the state database.
#[auto_impl(&, Box, Arc)]
pub trait StateDBRead {
    /// latest block hash
    fn read_latest_block_hash(&self) -> Result<H256, StorageError>;

    /// block hash -> block info
    fn read_block_info(&self, block_hash: H256) -> Result<Option<BlockInfo>, StorageError>;

    /// block num -> block hash
    fn read_block_hash(&self, block_num: u64) -> Result<H256, StorageError>;

    /// account address -> raw account
    fn read_account(&self, address: H256) -> Result<Option<NewAccount>, StorageError>;

    /// code hash -> code
    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, StorageError>;

    /// account address | storage index -> storage value
    fn read_storage(&self, address: H256, key: H256) -> Result<U256, StorageError>;

    /// Batched [`StateDBRead::read_account`]: one result per input, same
    /// order. The scalar default keeps archive/MDBX/mock backends correct;
    /// exact-key backends (non-archive RocksDB) override it with MultiGet.
    fn read_account_many(
        &self,
        addresses: &[H256],
    ) -> Result<Vec<Option<NewAccount>>, StorageError> {
        addresses
            .iter()
            .map(|address| self.read_account(*address))
            .collect()
    }

    /// Batched [`StateDBRead::read_code`]: one result per input, same order.
    fn read_code_many(&self, code_hashes: &[H256]) -> Result<Vec<Option<Bytes>>, StorageError> {
        code_hashes
            .iter()
            .map(|code_hash| self.read_code(*code_hash))
            .collect()
    }

    /// Batched [`StateDBRead::read_storage`] over `(address, key)` pairs:
    /// one result per input, same order.
    fn read_storage_many(&self, keys: &[(H256, H256)]) -> Result<Vec<U256>, StorageError> {
        keys.iter()
            .map(|(address, key)| self.read_storage(*address, *key))
            .collect()
    }

    /// Whether the `*_many` reads above are served by a real batched
    /// storage primitive (RocksDB MultiGet) instead of the scalar
    /// defaults. A performance hint for callers deciding whether eager
    /// batched reads are worth issuing; correctness never depends on it.
    fn supports_batched_reads(&self) -> bool {
        false
    }

    /// The slots of `address` holding a non-zero value, with the value, or
    /// `None` if the backend cannot list them (only the RocksDB archive
    /// can). For writing out the storage an account lost when destroyed.
    fn read_storage_slots(
        &self,
        _address: H256,
    ) -> Result<Option<Vec<(H256, U256)>>, StorageError> {
        Ok(None)
    }
}

#[auto_impl(&, Box, Arc)]
pub trait LatestStateDBIterator: Send + Sync + 'static {
    /// account address -> raw account
    fn account_iter(&self) -> impl Iterator<Item = Result<(H256, NewAccount), StorageError>>;

    /// code hash -> code
    fn code_iter(&self) -> impl Iterator<Item = Result<(H256, Bytes), StorageError>>;

    /// account address | storage index -> storage value
    fn storage_iter(&self) -> impl Iterator<Item = Result<(H256, H256, U256), StorageError>>;
}

/// [`StateDBWrite`] offers write-only access to the state database.
#[auto_impl(& , Box, Arc)]
pub trait StateDBWrite: Send + Sync + 'static {
    type DBWriteBatch: Send;

    /// prepare write batch for write
    fn prepare_write_batch(&self) -> Result<Self::DBWriteBatch, StorageError>;

    /// latest block hash
    fn write_latest_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_hash: H256,
    ) -> Result<(), StorageError>;

    /// block hash -> block info
    fn write_block_info(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_info: BlockInfo,
    ) -> Result<(), StorageError>;

    /// block num -> block hash
    fn write_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_num: u64, // only for archive db
        block_hash: H256,
    ) -> Result<(), StorageError>;

    /// account address -> raw account
    fn write_account(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        block_num: u64, // only for archive db
        raw_account: Option<NewAccount>,
    ) -> Result<(), StorageError>;

    /// code hash -> code
    fn write_code(
        &self,
        batch: &mut Self::DBWriteBatch,
        code_hash: H256,
        code: Bytes,
    ) -> Result<(), StorageError>;

    /// account address | storage index -> storage value
    fn write_storage(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        key: H256,
        block_num: u64,
        value: U256,
    ) -> Result<(), StorageError>;

    /// Clear the storage of `address`, deleted at `block_num`, for backends
    /// that keep it otherwise (archive).
    fn clear_storage(
        &self,
        _batch: &mut Self::DBWriteBatch,
        _address: H256,
        _block_num: u64,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    /// commit write batch
    fn commit(&self, batch: Self::DBWriteBatch) -> Result<(), StorageError>;
}

/// [`StateDBWrapper`] wraps a [`StateDBRead`] to implements [`BlockContext`]、[`StateDB`] and [`EvmStorageWrite`].
#[derive(Debug)]
pub struct StateDBWrapper<T>(pub T);

impl<T> Clone for StateDBWrapper<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> BlockContext for StateDBWrapper<T>
where
    T: StateDBRead,
{
    type Error = StorageError;

    fn block_info(&self) -> Result<BlockInfo, Self::Error> {
        let latest_block_hash = self.0.read_latest_block_hash()?;
        Ok(self.0.read_block_info(latest_block_hash)?.unwrap())
    }
}

impl<T> StateDB for StateDBWrapper<T>
where
    T: StateDBRead,
{
    type Error = StorageError;

    fn basic(&self, address: H256) -> Result<Option<AccountInfo>, Self::Error> {
        let raw_account_info = self.0.read_account(address.into())?;
        if let Some(raw_account_info) = raw_account_info {
            Ok(Some(raw_account_info.into()))
        } else {
            Ok(None)
        }
    }
    /// Get account code by its hash
    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        if let Some(bytecode) = ANALYZED_CODE_CACHE.as_ref().and_then(|c| c.get(&code_hash)) {
            return Ok(bytecode);
        }
        let code = self.0.read_code(code_hash.into())?;
        if let Some(code) = code {
            Ok(analyze_and_cache(code_hash, code))
        } else {
            Ok(Bytecode::default())
        }
    }
    /// Get storage value of address at index.
    fn storage(&self, address: H256, index: H256) -> Result<U256, Self::Error> {
        self.0.read_storage(address.into(), index)
    }

    // History related
    fn block_hash(&self, number: u64) -> Result<H256, Self::Error> {
        Ok(self.0.read_block_hash(number)?.into())
    }

    fn basic_many(&self, addresses: &[H256]) -> Result<Vec<Option<AccountInfo>>, Self::Error> {
        Ok(self
            .0
            .read_account_many(addresses)?
            .into_iter()
            .map(|raw| raw.map(Into::into))
            .collect())
    }

    fn code_by_hash_many(&self, code_hashes: &[H256]) -> Result<Vec<Bytecode>, Self::Error> {
        let cache = ANALYZED_CODE_CACHE.as_ref();
        let mut out: Vec<Option<Bytecode>> = code_hashes
            .iter()
            .map(|hash| cache.and_then(|c| c.get(hash)))
            .collect();
        let missing: Vec<H256> = code_hashes
            .iter()
            .zip(&out)
            .filter(|(_, hit)| hit.is_none())
            .map(|(hash, _)| *hash)
            .collect();
        if !missing.is_empty() {
            let mut read = self.0.read_code_many(&missing)?.into_iter();
            for (slot, hash) in out.iter_mut().zip(code_hashes) {
                if slot.is_none() {
                    *slot = Some(match read.next().flatten() {
                        Some(code) => analyze_and_cache(*hash, code),
                        None => Bytecode::default(),
                    });
                }
            }
        }
        Ok(out.into_iter().map(|b| b.unwrap_or_default()).collect())
    }

    fn storage_many(&self, keys: &[(H256, H256)]) -> Result<Vec<U256>, Self::Error> {
        self.0.read_storage_many(keys)
    }

    fn supports_batched_reads(&self) -> bool {
        self.0.supports_batched_reads()
    }

    fn storage_slots(&self, address: H256) -> Result<Option<Vec<(H256, U256)>>, Self::Error> {
        self.0.read_storage_slots(address)
    }
}

impl<T> EvmStorageWrite for StateDBWrapper<T>
where
    T: StateDBWrite + StateDBRead,
{
    type Error = StorageError;

    fn update_block(
        &self,
        block_info: BlockInfo,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error> {
        let start = std::time::Instant::now();
        let mut batch = self.0.prepare_write_batch()?;
        let block_number = block_info.header.number;
        // Correlate with the fetch-side logs in s3_get_block_diff via the
        // state root. Enable with RUST_LOG=state_diff=debug (or =trace for
        // per-account / per-slot detail).
        debug!(target: "state_diff",
            "commit block {} ({}): root {}, new_accounts {}, deleted_accounts {}, storage_accounts {}, storage_slots {}, new_codes {}",
            block_number,
            block_info.header.hash,
            block_info.header.state_root,
            block_diff.new_accounts.len(),
            block_diff.deleted_accounts.len(),
            block_diff.storage_diffs.len(),
            block_diff.storage_diffs.iter().map(|d| d.diffs.len()).sum::<usize>(),
            block_diff.new_codes.len(),
        );
        self.0
            .write_block_hash(&mut batch, block_info.header.number, block_info.header.hash)?;
        let hash = block_info.header.hash;
        self.0.write_block_info(&mut batch, block_info)?;
        for account in block_diff.deleted_accounts {
            trace!(target: "state_diff",
                "commit delete account: block {}, address {}", block_number, account);
            self.0
                .write_account(&mut batch, account, block_number, None)?;
            // The diff lists no storage for a deleted account; clear what it
            // held, before this block's own writes so that those win.
            self.0.clear_storage(&mut batch, account, block_number)?;
        }
        for account in block_diff.new_accounts {
            trace!(target: "state_diff",
                "commit account: block {}, address {}, balance {}, nonce {}, code_hash {}",
                block_number, account.address, account.balance, account.nonce, account.code_hash);
            self.0
                .write_account(&mut batch, account.address, block_number, Some(account))?;
        }
        for account_diff in block_diff.storage_diffs {
            for index_value_pair in account_diff.diffs {
                trace!(target: "state_diff",
                    "commit storage: block {}, address {}, index {}, value {}",
                    block_number, account_diff.address, index_value_pair.index, index_value_pair.value);
                self.0.write_storage(
                    &mut batch,
                    account_diff.address,
                    index_value_pair.index,
                    block_number,
                    index_value_pair.value,
                )?;
            }
        }
        for new_code in block_diff.new_codes {
            trace!(target: "state_diff",
                "commit code: block {}, code_hash {}, len {}",
                block_number, new_code.code_hash, new_code.code.len());
            self.0
                .write_code(&mut batch, new_code.code_hash, new_code.code)?;
        }
        self.0.write_latest_block_hash(&mut batch, hash)?;
        self.0.commit(batch)?;
        STORAGE_METRICS
            .commit_block_latency
            .record(start.elapsed().as_secs_f64());
        STORAGE_METRICS.latest_commit_block.set(block_number as f64);
        Ok(())
    }

    fn last_committed_block(&self) -> Result<Option<BlockInfo>, Self::Error> {
        let latest_block_hash = self.0.read_latest_block_hash()?;
        Ok(self.0.read_block_info(latest_block_hash)?)
    }
}

/// [`StateDBProvider`] offers read-only access to the archive database.
#[auto_impl(&, Box, Arc)]
pub trait StateDBProvider: Send + Sync + 'static {
    type StateDBReadWrite: StateDBRead + StateDBWrite + Send + Sync + Clone + Debug + 'static;
    fn db_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDBReadWrite>, StorageError>;

    fn state_at(
        &self,
        block_arg: BlockId,
    ) -> Result<Option<StateDBWrapper<Self::StateDBReadWrite>>, StorageError> {
        let db = self.db_at(block_arg)?;
        Ok(db.map(|db| StateDBWrapper(db)))
    }
}

#[cfg(test)]
mod analyzed_code_cache_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A code-only reader that counts how often the backend is consulted.
    #[derive(Default)]
    struct CodeOnly {
        codes: HashMap<H256, Bytes>,
        reads: AtomicUsize,
    }

    impl StateDBRead for CodeOnly {
        fn read_latest_block_hash(&self) -> Result<H256, StorageError> {
            unimplemented!()
        }
        fn read_block_info(&self, _: H256) -> Result<Option<BlockInfo>, StorageError> {
            unimplemented!()
        }
        fn read_block_hash(&self, _: u64) -> Result<H256, StorageError> {
            unimplemented!()
        }
        fn read_account(&self, _: H256) -> Result<Option<NewAccount>, StorageError> {
            unimplemented!()
        }
        fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, StorageError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(self.codes.get(&code_hash).cloned())
        }
        fn read_storage(&self, _: H256, _: H256) -> Result<U256, StorageError> {
            unimplemented!()
        }
    }

    fn hash(tag: u8) -> H256 {
        // Distinct from every other test's keys: the cache is process-wide.
        H256::from_slice(&[0xA7, 0xC0, 0xDE, tag].repeat(8))
    }

    /// The cache is configured once per process from the environment.
    fn cache_enabled() -> bool {
        ANALYZED_CODE_CACHE.is_some()
    }

    #[test]
    fn repeat_code_reads_reuse_the_analyzed_bytecode() {
        // PUSH1 4, JUMP, INVALID, JUMPDEST, STOP
        let code = Bytes::from(vec![0x60, 0x04, 0x56, 0xfe, 0x5b, 0x00]);
        let h = hash(1);
        let reader = CodeOnly {
            codes: HashMap::from([(h, code.clone())]),
            ..Default::default()
        };
        let db = StateDBWrapper(reader);
        let first = db.code_by_hash(h).unwrap();
        let second = db.code_by_hash(h).unwrap();
        assert_eq!(first, Bytecode::new_raw(code.0.clone().into()));
        assert_eq!(first, second);
        assert!(first.legacy_jump_table().unwrap().is_valid(4));
        let expected_reads = if cache_enabled() { 1 } else { 2 };
        assert_eq!(db.0.reads.load(Ordering::Relaxed), expected_reads);
    }

    #[test]
    fn missing_code_is_empty_and_not_remembered() {
        let h = hash(2);
        let db = StateDBWrapper(CodeOnly::default());
        assert_eq!(db.code_by_hash(h).unwrap(), Bytecode::default());
        assert_eq!(db.code_by_hash(h).unwrap(), Bytecode::default());
        assert_eq!(db.0.reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn batched_reads_match_scalar_reads_and_fill_the_cache() {
        let (a, b, missing) = (hash(3), hash(4), hash(5));
        let code_a = Bytes::from(vec![0x60, 0x01, 0x00]);
        let code_b = Bytes::from(vec![0x5b, 0x60, 0x00, 0x56]);
        let reader = CodeOnly {
            codes: HashMap::from([(a, code_a.clone()), (b, code_b.clone())]),
            ..Default::default()
        };
        let db = StateDBWrapper(reader);
        // Warm only `a`, then ask for a mixed batch with a duplicate.
        db.code_by_hash(a).unwrap();
        let batch = db.code_by_hash_many(&[b, a, missing, b]).unwrap();
        assert_eq!(
            batch,
            vec![
                Bytecode::new_raw(code_b.0.clone().into()),
                Bytecode::new_raw(code_a.0.clone().into()),
                Bytecode::default(),
                Bytecode::new_raw(code_b.0.clone().into()),
            ]
        );
        let reads = db.0.reads.load(Ordering::Relaxed);
        db.code_by_hash(b).unwrap();
        let expected_reads = if cache_enabled() { reads } else { reads + 1 };
        assert_eq!(db.0.reads.load(Ordering::Relaxed), expected_reads);
    }
}
