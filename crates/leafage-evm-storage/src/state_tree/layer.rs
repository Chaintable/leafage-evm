use crate::interface::{BlockContext, EvmStorageWrite, StateDB};
use crate::state_tree::error::Error;
use leafage_evm_types::{AccountInfo, BlockInfo, BlockStorageDiff, Bytecode, H256, U256};
use moka::ops::compute::Op;
use moka::sync::Cache;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use tracing::info;

/// [`LinkedDiffLayer`] is a single linked list that stores the state of the EVM.
///
/// two cases:
/// 1. bottom [`CacheDiskLayer`] (when init).
/// 2. top [`DiffLayer`] -> (top-1) [`DiffLayer`] -> ... -> bottom [`CacheDiskLayer`].
#[derive(Debug)]
pub enum LinkedDiffLayer {
    CacheDiskLayer(CacheDiskLayer),
    DiffLayer(DiffLayer),
    Empty,
}

impl LinkedDiffLayer {
    /// commit the diff layer to the db and return the bottom block number.
    pub fn cap_diff_to_db<DB, E>(
        self: Arc<Self>,
        depth_limit: usize,
        statedb: DB,
    ) -> Result<u64, Error<E>>
    where
        DB: EvmStorageWrite<Error = E> + BlockContext<Error = E>,
    {
        let cur = self;
        let mut diff_layers = VecDeque::new();
        diff_layers.push_back(cur);
        loop {
            let cur = diff_layers.back().unwrap();
            if cur.is_cache_layer() {
                break;
            }
            let next = cur.unwrap_diff_layer().next.read().unwrap().clone();
            diff_layers.push_back(next);
        }
        let cache_layer = diff_layers.pop_back().unwrap();
        let mut bottom_num: u64 = 0;
        while diff_layers.len() > depth_limit {
            let diff_layer = diff_layers.pop_back().unwrap();
            // commit the diff layer to the db
            bottom_num = diff_layer.unwrap_diff_layer().block_info.header.number;
            cache_layer
                .unwrap_cache_layer()
                .commit(diff_layer, &statedb)?;
            let next_diff_layer = diff_layers.back().unwrap();
            *next_diff_layer.unwrap_diff_layer().next.write().unwrap() = cache_layer.clone();
        }
        if bottom_num == 0 {
            // Metadata-only handle (block_info_arc). `None` disables cache
            // refills; block_info_arc itself reads old_diff_layer / statedb.
            bottom_num = HybridStateDB::new(cache_layer.clone(), statedb, None)
                .block_info_arc()?
                .header
                .number;
        }
        Ok(bottom_num)
    }
}

impl LinkedDiffLayer {
    #[inline]
    pub fn is_diff_layer(&self) -> bool {
        match self {
            LinkedDiffLayer::DiffLayer(_) => true,
            _ => false,
        }
    }

    #[inline]
    pub fn is_cache_layer(&self) -> bool {
        match self {
            LinkedDiffLayer::CacheDiskLayer(_) => true,
            _ => false,
        }
    }

    #[inline]
    pub fn diff_layer(&self) -> Option<&DiffLayer> {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => Some(diff),
            _ => None,
        }
    }

    #[inline]
    pub fn unwrap_diff_layer(&self) -> &DiffLayer {
        match self {
            LinkedDiffLayer::DiffLayer(diff) => diff,
            _ => panic!("unwrap_diff_layer"),
        }
    }

    #[inline]
    pub fn cache_layer(&self) -> Option<&CacheDiskLayer> {
        match self {
            LinkedDiffLayer::CacheDiskLayer(cache) => Some(cache),
            _ => None,
        }
    }

    #[inline]
    pub fn unwrap_cache_layer(&self) -> &CacheDiskLayer {
        match self {
            LinkedDiffLayer::CacheDiskLayer(cache) => cache,
            _ => panic!("unwrap_cache_layer"),
        }
    }
}

/// [`CacheDiskLayer`] is the bottom layer of the linked list. It backs the
/// on-disk DB and caches recently committed state.
///
/// Each entry is tagged with a block height (commit block_num for
/// write-through, handle snapshot for refill). Both paths go through
/// `entry().and_compute_with` so the "only overwrite if new tag is
/// strictly higher" rule holds atomically — no TOCTOU window between
/// the tag check and the write.
#[derive(Debug)]
pub struct CacheDiskLayer {
    accounts: Cache<H256, (u64, Option<AccountInfo>)>,
    storages: Cache<(H256, H256), (u64, U256)>,
    contracts: Cache<H256, (u64, Bytecode)>,
    block_hashes: Cache<u64, (u64, H256)>,
    old_diff_layer: Mutex<Option<Arc<LinkedDiffLayer>>>,
    /// Latest committed block height. Published after write-through so
    /// handles snapshotting it find the diff's keys already tagged.
    committed_height: AtomicU64,
    /// When false the moka caches are never read or written. Backends
    /// with per-handle snapshot isolation (MDBX) set this to skip the
    /// shared cache entirely.
    enabled: bool,
}

impl CacheDiskLayer {
    pub fn new(
        accounts_cache_size: usize,
        storage_cache_size: usize,
        contract_cache_size: usize,
        initial_committed_height: u64,
        enabled: bool,
    ) -> Self {
        // When disabled, build with capacity 0; the caches are never
        // consulted but the fields still need valid values.
        let (a, s, c, b) = if enabled {
            (accounts_cache_size as u64, storage_cache_size as u64, contract_cache_size as u64, 1_000)
        } else {
            (0, 0, 0, 0)
        };
        Self {
            accounts: Cache::builder().max_capacity(a).build(),
            storages: Cache::builder().max_capacity(s).build(),
            contracts: Cache::builder().max_capacity(c).build(),
            block_hashes: Cache::builder().max_capacity(b).build(),
            old_diff_layer: Mutex::new(None),
            committed_height: AtomicU64::new(initial_committed_height),
            enabled,
        }
    }

    /// Refill tag for handles built on top of this layer. `None` when
    /// the cache is disabled so callers skip refill entirely.
    #[inline]
    pub fn cache_view_height(&self) -> Option<u64> {
        if self.enabled {
            Some(self.committed_height.load(Ordering::Acquire))
        } else {
            None
        }
    }

    #[inline]
    fn old_diff_layer_lock(&self) -> MutexGuard<'_, Option<Arc<LinkedDiffLayer>>> {
        self.old_diff_layer
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    pub fn commit<DB, E>(&self, diff_layer: Arc<LinkedDiffLayer>, db: &DB) -> Result<(), E>
    where
        DB: EvmStorageWrite<Error = E> + BlockContext<Error = E>,
    {
        let diff = diff_layer.unwrap_diff_layer();
        let block_num = diff.block_info.header.number;
        let (block_info, block_diff) = diff.storage_diff();
        info!(target: "storage",
            "commit diff layer to db, block number: {}, block hash: {}, account cache size: {}, storage cache size: {}, contract cache size: {}",
            block_info.header.number, block_info.header.hash,
            self.accounts.entry_count(), self.storages.entry_count(), self.contracts.entry_count()
        );

        let old_head = self.old_diff_layer_lock().clone();
        if let Some(old_head) = old_head {
            assert_eq!(
                old_head.unwrap_diff_layer().block_info.header.hash,
                diff.block_info.header.parent_hash
            );
        }

        // DB write is expected atomic; on Err nothing below runs.
        db.update_block(block_info, block_diff)?;
        *self.old_diff_layer_lock() = Some(diff_layer.clone());

        if self.enabled {
            // Unconditional write-through. Safe because (a) commits are
            // serialized so block_num is monotonically increasing, and
            // (b) any existing tag at this point is < block_num (refills
            // tag with committed_height which has not yet advanced to
            // block_num) — so a CAS would always pass.
            for (key, value) in diff.accounts.iter() {
                self.accounts.insert(*key, (block_num, value.clone()));
            }
            for (key, value) in diff.storage.iter() {
                self.storages.insert(*key, (block_num, *value));
            }
            for (key, value) in diff.contracts.iter() {
                self.contracts.insert(*key, (block_num, value.clone()));
            }
            self.block_hashes
                .insert(block_num, (block_num, diff.block_info.header.hash));
        }

        // Publish AFTER write-through: later handles snapshot block_num
        // and find diff keys already tagged, so their refills skip.
        self.committed_height
            .store(block_num, Ordering::Release);

        Ok(())
    }
}

/// [`DiffLayer`] is the top layer of the linked list.
/// It stores the diff of the EVM.
#[derive(Debug)]
pub struct DiffLayer {
    pub block_info: Arc<BlockInfo>,
    pub block_diff: Arc<BlockStorageDiff>,
    pub accounts: HashMap<H256, Option<AccountInfo>>,
    pub storage: HashMap<(H256, H256), U256>,
    pub contracts: HashMap<H256, Bytecode>,
    pub next: RwLock<Arc<LinkedDiffLayer>>,
}

impl From<(BlockInfo, BlockStorageDiff, Arc<LinkedDiffLayer>)> for DiffLayer {
    fn from(
        (block_info, block_diff, db): (BlockInfo, BlockStorageDiff, Arc<LinkedDiffLayer>),
    ) -> Self {
        Self::new(block_info, block_diff, db)
    }
}

impl Into<(BlockInfo, BlockStorageDiff)> for &DiffLayer {
    fn into(self) -> (BlockInfo, BlockStorageDiff) {
        self.storage_diff()
    }
}

impl DiffLayer {
    pub fn new(
        block_info: BlockInfo,
        block_diff: BlockStorageDiff,
        next: Arc<LinkedDiffLayer>,
    ) -> Self {
        let mut accounts = HashMap::new();
        let mut storage = HashMap::new();
        let mut contracts = HashMap::new();
        for del_account in block_diff.deleted_accounts.iter() {
            accounts.insert(del_account.clone(), None);
        }
        for new_account in block_diff.new_accounts.iter() {
            let address = new_account.address;
            let account_info: AccountInfo = new_account.clone().into();
            accounts.insert(address, Some(account_info));
        }
        for account_diff in block_diff.storage_diffs.iter() {
            let address = account_diff.address;
            for index_value in account_diff.diffs.iter() {
                storage.insert((address, index_value.index), index_value.value);
            }
        }
        for new_code in block_diff.new_codes.iter() {
            let code_hash = new_code.code_hash;
            let code = Bytecode::new_raw(new_code.code.0.clone().into());
            contracts.insert(code_hash, code);
        }
        Self {
            block_info: Arc::new(block_info),
            block_diff: Arc::new(block_diff),
            accounts,
            storage,
            contracts,
            next: RwLock::new(next),
        }
    }

    fn storage_diff(&self) -> (BlockInfo, BlockStorageDiff) {
        (
            self.block_info.as_ref().clone(),
            self.block_diff.as_ref().clone(),
        )
    }
}

#[derive(Debug, Clone)]
pub struct HybridStateDB<DB> {
    pub memory_layer: Arc<LinkedDiffLayer>,
    pub statedb: DB,
    /// `Some(h)` enables cache refill on miss, tagging the insert with
    /// `h` and only overwriting strictly-lower-tagged entries. `None`
    /// disables refill (metadata-only handles).
    pub cache_view_height: Option<u64>,
}

impl<DB> HybridStateDB<DB> {
    pub fn new(
        memory_layer: Arc<LinkedDiffLayer>,
        statedb: DB,
        cache_view_height: Option<u64>,
    ) -> Self {
        Self {
            memory_layer,
            statedb,
            cache_view_height,
        }
    }
}

impl<DB: StateDB> StateDB for HybridStateDB<DB> {
    type Error = Error<DB::Error>;

    fn basic(&self, address: H256) -> Result<Option<AccountInfo>, Self::Error> {
        Self::basic_from_layer(&self.memory_layer, address, &self.statedb, self.cache_view_height)
    }

    fn storage(&self, address: H256, index: H256) -> Result<U256, Self::Error> {
        Self::storage_from_layer(
            &self.memory_layer,
            address,
            index,
            &self.statedb,
            self.cache_view_height,
        )
    }

    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        Self::code_from_layer(&self.memory_layer, code_hash, &self.statedb, self.cache_view_height)
    }

    fn block_hash(&self, number: u64) -> Result<H256, Self::Error> {
        Self::block_hash_from_layer(&self.memory_layer, number, &self.statedb, self.cache_view_height)
    }
}

impl<DB: StateDB> HybridStateDB<DB> {
    /// Helper function to recursively search for account in layers
    #[inline]
    fn basic_from_layer(
        layer: &Arc<LinkedDiffLayer>,
        address: H256,
        statedb: &DB,
        cache_view_height: Option<u64>,
    ) -> Result<Option<AccountInfo>, Error<DB::Error>> {
        match layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => match diff.accounts.get(&address) {
                Some(value) => Ok(value.clone()),
                None => {
                    let next = diff
                        .next
                        .read()
                        .expect("Failed to acquire read lock on diff layer");
                    Self::basic_from_layer(&next, address, statedb, cache_view_height)
                }
            },
            LinkedDiffLayer::CacheDiskLayer(cache) if !cache.enabled => {
                Ok(statedb.basic(address)?)
            }
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.accounts.get(&address) {
                Some((_, value)) => Ok(value),
                None => {
                    let res = statedb.basic(address)?;
                    if let Some(h) = cache_view_height {
                        let new_val = (h, res.clone());
                        cache.accounts.entry(address).and_compute_with(|maybe| match maybe {
                            Some(e) if e.value().0 >= h => Op::Nop,
                            _ => Op::Put(new_val),
                        });
                    }
                    Ok(res)
                }
            },
            LinkedDiffLayer::Empty => Ok(statedb.basic(address)?),
        }
    }

    /// Helper function to recursively search for storage in layers
    #[inline]
    fn storage_from_layer(
        layer: &Arc<LinkedDiffLayer>,
        address: H256,
        index: H256,
        statedb: &DB,
        cache_view_height: Option<u64>,
    ) -> Result<U256, Error<DB::Error>> {
        match layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => match diff.storage.get(&(address, index)) {
                Some(value) => Ok(*value),
                None => {
                    let next = diff
                        .next
                        .read()
                        .expect("Failed to acquire read lock on diff layer");
                    Self::storage_from_layer(&next, address, index, statedb, cache_view_height)
                }
            },
            LinkedDiffLayer::CacheDiskLayer(cache) if !cache.enabled => {
                Ok(statedb.storage(address, index)?)
            }
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.storages.get(&(address, index)) {
                Some((_, value)) => Ok(value),
                None => {
                    let res = statedb.storage(address, index)?;
                    if let Some(h) = cache_view_height {
                        let new_val = (h, res);
                        cache.storages.entry((address, index)).and_compute_with(|maybe| match maybe {
                            Some(e) if e.value().0 >= h => Op::Nop,
                            _ => Op::Put(new_val),
                        });
                    }
                    Ok(res)
                }
            },
            LinkedDiffLayer::Empty => Ok(statedb.storage(address, index)?),
        }
    }

    /// Helper function to recursively search for code in layers
    #[inline]
    fn code_from_layer(
        layer: &Arc<LinkedDiffLayer>,
        code_hash: H256,
        statedb: &DB,
        cache_view_height: Option<u64>,
    ) -> Result<Bytecode, Error<DB::Error>> {
        match layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => match diff.contracts.get(&code_hash) {
                Some(value) => Ok(value.clone()),
                None => {
                    let next = diff
                        .next
                        .read()
                        .expect("Failed to acquire read lock on diff layer");
                    Self::code_from_layer(&next, code_hash, statedb, cache_view_height)
                }
            },
            LinkedDiffLayer::CacheDiskLayer(cache) if !cache.enabled => {
                Ok(statedb.code_by_hash(code_hash)?)
            }
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.contracts.get(&code_hash) {
                Some((_, value)) => Ok(value),
                None => {
                    let res = statedb.code_by_hash(code_hash)?;
                    if let Some(h) = cache_view_height {
                        let new_val = (h, res.clone());
                        cache.contracts.entry(code_hash).and_compute_with(|maybe| match maybe {
                            Some(e) if e.value().0 >= h => Op::Nop,
                            _ => Op::Put(new_val),
                        });
                    }
                    Ok(res)
                }
            },
            LinkedDiffLayer::Empty => Ok(statedb.code_by_hash(code_hash)?),
        }
    }

    /// Helper function to recursively search for block hash in layers
    #[inline]
    fn block_hash_from_layer(
        layer: &Arc<LinkedDiffLayer>,
        number: u64,
        statedb: &DB,
        cache_view_height: Option<u64>,
    ) -> Result<H256, Error<DB::Error>> {
        match layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => {
                if diff.block_info.header.number == number {
                    Ok(diff.block_info.header.hash)
                } else {
                    let next = diff
                        .next
                        .read()
                        .expect("Failed to acquire read lock on diff layer");
                    Self::block_hash_from_layer(&next, number, statedb, cache_view_height)
                }
            }
            LinkedDiffLayer::CacheDiskLayer(cache) if !cache.enabled => {
                Ok(statedb.block_hash(number)?)
            }
            LinkedDiffLayer::CacheDiskLayer(cache) => match cache.block_hashes.get(&number) {
                Some((_, value)) => Ok(value),
                None => {
                    let res = statedb.block_hash(number)?;
                    if let Some(h) = cache_view_height {
                        let new_val = (h, res);
                        cache.block_hashes.entry(number).and_compute_with(|maybe| match maybe {
                            Some(e) if e.value().0 >= h => Op::Nop,
                            _ => Op::Put(new_val),
                        });
                    }
                    Ok(res)
                }
            },
            LinkedDiffLayer::Empty => Ok(statedb.block_hash(number)?),
        }
    }
}

impl<StateDB: BlockContext> BlockContext for HybridStateDB<StateDB> {
    type Error = Error<StateDB::Error>;

    fn block_info_arc(&self) -> Result<Arc<BlockInfo>, Self::Error> {
        match self.memory_layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => Ok(diff.block_info.clone()),
            LinkedDiffLayer::CacheDiskLayer(cache) => {
                let last_diff = cache.old_diff_layer_lock().clone();
                if let Some(last_diff) = last_diff {
                    return Ok(last_diff.unwrap_diff_layer().block_info.clone());
                }
                let res = self.statedb.block_info_arc()?;
                Ok(res)
            }
            LinkedDiffLayer::Empty => {
                let res = self.statedb.block_info_arc()?;
                Ok(res)
            }
        }
    }

    fn state_diff_arc(&self) -> Result<Arc<BlockStorageDiff>, Self::Error> {
        match self.memory_layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => Ok(diff.block_diff.clone()),
            LinkedDiffLayer::CacheDiskLayer(cache) => {
                let last_diff = cache.old_diff_layer_lock().clone();
                if let Some(last_diff) = last_diff {
                    return Ok(last_diff.unwrap_diff_layer().block_diff.clone());
                }
                let res = self.statedb.state_diff_arc()?;
                Ok(res)
            }
            LinkedDiffLayer::Empty => {
                let res = self.statedb.state_diff_arc()?;
                Ok(res)
            }
        }
    }
}
