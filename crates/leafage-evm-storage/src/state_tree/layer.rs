use crate::interface::{BlockContext, EvmStorageWrite, StateDB};
use crate::state_tree::error::Error;
use leafage_evm_types::{AccountInfo, BlockInfo, BlockStorageDiff, Bytecode, H256, U256};
use moka::ops::compute::Op;
use moka::sync::Cache;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock};
use tracing::info;

/// Keys are already uniformly distributed (keccak-derived), and these
/// maps sit on the per-read hot path — use ahash instead of SipHash.
type FastMap<K, V> = HashMap<K, V, ahash::RandomState>;
type FastCache<K, V> = Cache<K, V, ahash::RandomState>;

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
    accounts: FastCache<H256, (u64, Option<AccountInfo>)>,
    storages: FastCache<(H256, H256), (u64, U256)>,
    contracts: FastCache<H256, (u64, Bytecode)>,
    block_hashes: FastCache<u64, (u64, H256)>,
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
            (
                accounts_cache_size as u64,
                storage_cache_size as u64,
                contract_cache_size as u64,
                1_000,
            )
        } else {
            (0, 0, 0, 0)
        };
        Self {
            accounts: Cache::builder()
                .max_capacity(a)
                .build_with_hasher(ahash::RandomState::default()),
            storages: Cache::builder()
                .max_capacity(s)
                .build_with_hasher(ahash::RandomState::default()),
            contracts: Cache::builder()
                .max_capacity(c)
                .build_with_hasher(ahash::RandomState::default()),
            block_hashes: Cache::builder()
                .max_capacity(b)
                .build_with_hasher(ahash::RandomState::default()),
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
        self.committed_height.store(block_num, Ordering::Release);

        Ok(())
    }
}

/// [`DiffLayer`] is the top layer of the linked list.
/// It stores the diff of the EVM.
#[derive(Debug)]
pub struct DiffLayer {
    pub block_info: Arc<BlockInfo>,
    pub block_diff: Arc<BlockStorageDiff>,
    pub accounts: FastMap<H256, Option<AccountInfo>>,
    pub storage: FastMap<(H256, H256), U256>,
    pub contracts: FastMap<H256, Bytecode>,
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
        let mut accounts = FastMap::default();
        let mut storage = FastMap::default();
        let mut contracts = FastMap::default();
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

/// Immutable read index over a linked diff-layer head.
///
/// The linked representation remains the source of truth for parent/fork/cap
/// topology. StateTree builds one of these views when it imports a block, and
/// every state handle for that block shares it through an Arc.
#[derive(Debug)]
pub(crate) struct FlattenedLayerView {
    /// Diff layers from the head (inclusive) down to the terminal layer
    /// (exclusive), ordered newest to oldest.
    layers: Box<[Arc<LinkedDiffLayer>]>,
    /// The layer the chain bottoms out in: CacheDiskLayer or Empty.
    terminal: Arc<LinkedDiffLayer>,
}

impl FlattenedLayerView {
    pub(crate) fn build(memory_layer: Arc<LinkedDiffLayer>) -> Arc<Self> {
        let mut layers = Vec::new();
        let mut cur = memory_layer;
        while let LinkedDiffLayer::DiffLayer(diff) = cur.as_ref() {
            let next = diff
                .next
                .read()
                .expect("Failed to acquire read lock on diff layer")
                .clone();
            layers.push(cur);
            cur = next;
        }
        Arc::new(Self {
            layers: layers.into_boxed_slice(),
            terminal: cur,
        })
    }

    #[inline]
    pub(crate) fn head(&self) -> &Arc<LinkedDiffLayer> {
        self.layers.first().unwrap_or(&self.terminal)
    }

    pub(crate) fn empty() -> Arc<Self> {
        static EMPTY: LazyLock<Arc<FlattenedLayerView>> =
            LazyLock::new(|| FlattenedLayerView::build(Arc::new(LinkedDiffLayer::Empty)));
        EMPTY.clone()
    }

    #[cfg(test)]
    pub(crate) fn diff_layer_count(&self) -> usize {
        self.layers.len()
    }
}

#[derive(Debug, Clone)]
pub struct HybridStateDB<DB> {
    pub(crate) flattened: Arc<FlattenedLayerView>,
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
        let flattened = FlattenedLayerView::build(memory_layer);
        Self::from_flattened(flattened, statedb, cache_view_height)
    }

    pub(crate) fn from_flattened(
        flattened: Arc<FlattenedLayerView>,
        statedb: DB,
        cache_view_height: Option<u64>,
    ) -> Self {
        Self {
            flattened,
            statedb,
            cache_view_height,
        }
    }
}

impl<DB: StateDB> HybridStateDB<DB> {
    /// Shared walk for the batched reads: resolve each key through the
    /// diff layers and the shared cache first, fetch the remaining
    /// unique keys from the bottom DB in one batched call, refill the
    /// cache under the same strictly-higher-tag rule as the scalar
    /// paths, and restore input order (duplicates share one fetch).
    fn read_many_layered<K, V>(
        &self,
        keys: &[K],
        diff_lookup: impl Fn(&DiffLayer, &K) -> Option<V>,
        cache_lookup: impl Fn(&CacheDiskLayer, &K) -> Option<V>,
        cache_fill: impl Fn(&CacheDiskLayer, &K, u64, &V),
        fetch_many: impl Fn(&[K]) -> Result<Vec<V>, DB::Error>,
    ) -> Result<Vec<V>, Error<DB::Error>>
    where
        K: Copy + Eq + std::hash::Hash,
        V: Clone,
    {
        let cache = match self.flattened.terminal.as_ref() {
            LinkedDiffLayer::CacheDiskLayer(cache) if cache.enabled => Some(cache),
            _ => None,
        };
        let mut out: Vec<Option<V>> = vec![None; keys.len()];
        let mut unresolved: Vec<K> = Vec::new();
        let mut unresolved_slots: FastMap<K, Vec<usize>> = FastMap::default();
        'keys: for (pos, key) in keys.iter().enumerate() {
            for layer in self.flattened.layers.iter() {
                if let Some(value) = diff_lookup(layer.unwrap_diff_layer(), key) {
                    out[pos] = Some(value);
                    continue 'keys;
                }
            }
            if let Some(cache) = cache {
                if let Some(value) = cache_lookup(cache, key) {
                    out[pos] = Some(value);
                    continue 'keys;
                }
            }
            unresolved_slots
                .entry(*key)
                .or_insert_with(|| {
                    unresolved.push(*key);
                    Vec::new()
                })
                .push(pos);
        }
        if !unresolved.is_empty() {
            let fetched = fetch_many(&unresolved)?;
            debug_assert_eq!(fetched.len(), unresolved.len());
            for (key, value) in unresolved.iter().zip(fetched) {
                if let (Some(cache), Some(h)) = (cache, self.cache_view_height) {
                    cache_fill(cache, key, h, &value);
                }
                for pos in unresolved_slots.get(key).into_iter().flatten() {
                    out[*pos] = Some(value.clone());
                }
            }
        }
        Ok(out.into_iter().map(|value| value.unwrap()).collect())
    }
}

impl<DB: StateDB> StateDB for HybridStateDB<DB> {
    type Error = Error<DB::Error>;

    fn basic(&self, address: H256) -> Result<Option<AccountInfo>, Self::Error> {
        for layer in self.flattened.layers.iter() {
            if let Some(value) = layer.unwrap_diff_layer().accounts.get(&address) {
                return Ok(value.clone());
            }
        }
        match self.flattened.terminal.as_ref() {
            LinkedDiffLayer::CacheDiskLayer(cache) if cache.enabled => {
                match cache.accounts.get(&address) {
                    Some((_, value)) => Ok(value),
                    None => {
                        let res = self.statedb.basic(address)?;
                        if let Some(h) = self.cache_view_height {
                            let new_val = (h, res.clone());
                            cache
                                .accounts
                                .entry(address)
                                .and_compute_with(|maybe| match maybe {
                                    Some(e) if e.value().0 >= h => Op::Nop,
                                    _ => Op::Put(new_val),
                                });
                        }
                        Ok(res)
                    }
                }
            }
            _ => Ok(self.statedb.basic(address)?),
        }
    }

    fn storage(&self, address: H256, index: H256) -> Result<U256, Self::Error> {
        for layer in self.flattened.layers.iter() {
            if let Some(value) = layer.unwrap_diff_layer().storage.get(&(address, index)) {
                return Ok(*value);
            }
        }
        match self.flattened.terminal.as_ref() {
            LinkedDiffLayer::CacheDiskLayer(cache) if cache.enabled => {
                match cache.storages.get(&(address, index)) {
                    Some((_, value)) => Ok(value),
                    None => {
                        let res = self.statedb.storage(address, index)?;
                        if let Some(h) = self.cache_view_height {
                            let new_val = (h, res);
                            cache
                                .storages
                                .entry((address, index))
                                .and_compute_with(|maybe| match maybe {
                                    Some(e) if e.value().0 >= h => Op::Nop,
                                    _ => Op::Put(new_val),
                                });
                        }
                        Ok(res)
                    }
                }
            }
            _ => Ok(self.statedb.storage(address, index)?),
        }
    }

    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        for layer in self.flattened.layers.iter() {
            if let Some(value) = layer.unwrap_diff_layer().contracts.get(&code_hash) {
                return Ok(value.clone());
            }
        }
        match self.flattened.terminal.as_ref() {
            LinkedDiffLayer::CacheDiskLayer(cache) if cache.enabled => {
                match cache.contracts.get(&code_hash) {
                    Some((_, value)) => Ok(value),
                    None => {
                        let res = self.statedb.code_by_hash(code_hash)?;
                        if let Some(h) = self.cache_view_height {
                            let new_val = (h, res.clone());
                            cache.contracts.entry(code_hash).and_compute_with(
                                |maybe| match maybe {
                                    Some(e) if e.value().0 >= h => Op::Nop,
                                    _ => Op::Put(new_val),
                                },
                            );
                        }
                        Ok(res)
                    }
                }
            }
            _ => Ok(self.statedb.code_by_hash(code_hash)?),
        }
    }

    fn basic_many(&self, addresses: &[H256]) -> Result<Vec<Option<AccountInfo>>, Self::Error> {
        self.read_many_layered(
            addresses,
            |diff, key| diff.accounts.get(key).cloned(),
            |cache, key| cache.accounts.get(key).map(|(_, value)| value),
            |cache, key, h, value| {
                let new_val = (h, value.clone());
                cache
                    .accounts
                    .entry(*key)
                    .and_compute_with(|maybe| match maybe {
                        Some(e) if e.value().0 >= h => Op::Nop,
                        _ => Op::Put(new_val),
                    });
            },
            |keys| self.statedb.basic_many(keys),
        )
    }

    fn storage_many(&self, keys: &[(H256, H256)]) -> Result<Vec<U256>, Self::Error> {
        self.read_many_layered(
            keys,
            |diff, key| diff.storage.get(key).copied(),
            |cache, key| cache.storages.get(key).map(|(_, value)| value),
            |cache, key, h, value| {
                let new_val = (h, *value);
                cache
                    .storages
                    .entry(*key)
                    .and_compute_with(|maybe| match maybe {
                        Some(e) if e.value().0 >= h => Op::Nop,
                        _ => Op::Put(new_val),
                    });
            },
            |keys| self.statedb.storage_many(keys),
        )
    }

    fn code_by_hash_many(&self, code_hashes: &[H256]) -> Result<Vec<Bytecode>, Self::Error> {
        self.read_many_layered(
            code_hashes,
            |diff, key| diff.contracts.get(key).cloned(),
            |cache, key| cache.contracts.get(key).map(|(_, value)| value),
            |cache, key, h, value| {
                let new_val = (h, value.clone());
                cache
                    .contracts
                    .entry(*key)
                    .and_compute_with(|maybe| match maybe {
                        Some(e) if e.value().0 >= h => Op::Nop,
                        _ => Op::Put(new_val),
                    });
            },
            |keys| self.statedb.code_by_hash_many(keys),
        )
    }

    fn supports_batched_reads(&self) -> bool {
        self.statedb.supports_batched_reads()
    }

    fn block_hash(&self, number: u64) -> Result<H256, Self::Error> {
        for layer in self.flattened.layers.iter() {
            let diff = layer.unwrap_diff_layer();
            if diff.block_info.header.number == number {
                return Ok(diff.block_info.header.hash);
            }
        }
        match self.flattened.terminal.as_ref() {
            LinkedDiffLayer::CacheDiskLayer(cache) if cache.enabled => {
                match cache.block_hashes.get(&number) {
                    Some((_, value)) => Ok(value),
                    None => {
                        let res = self.statedb.block_hash(number)?;
                        if let Some(h) = self.cache_view_height {
                            let new_val = (h, res);
                            cache.block_hashes.entry(number).and_compute_with(
                                |maybe| match maybe {
                                    Some(e) if e.value().0 >= h => Op::Nop,
                                    _ => Op::Put(new_val),
                                },
                            );
                        }
                        Ok(res)
                    }
                }
            }
            _ => Ok(self.statedb.block_hash(number)?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::BlockIndex;
    use leafage_evm_types::{AccountStorageDiff, BlockId, IndexValuePair, NewAccount};
    use std::sync::Mutex as StdMutex;

    #[derive(Debug, thiserror::Error)]
    #[error("mock error")]
    struct MockErr;
    impl revm::database_interface::DBErrorMarker for MockErr {}

    /// In-memory bottom DB recording committed diffs.
    #[derive(Debug, Default, Clone)]
    struct MockDB {
        inner: Arc<StdMutex<MockDBInner>>,
    }

    #[derive(Debug, Default)]
    struct MockDBInner {
        storage: HashMap<(H256, H256), U256>,
        accounts: HashMap<H256, Option<AccountInfo>>,
        last_block: Option<Arc<BlockInfo>>,
        reads: u64,
    }

    impl MockDB {
        fn reads(&self) -> u64 {
            self.inner.lock().unwrap().reads
        }
    }

    impl StateDB for MockDB {
        type Error = MockErr;
        fn basic(&self, address: H256) -> Result<Option<AccountInfo>, MockErr> {
            let mut inner = self.inner.lock().unwrap();
            inner.reads += 1;
            Ok(inner.accounts.get(&address).cloned().flatten())
        }
        fn code_by_hash(&self, _code_hash: H256) -> Result<Bytecode, MockErr> {
            self.inner.lock().unwrap().reads += 1;
            Ok(Bytecode::default())
        }
        fn storage(&self, address: H256, index: H256) -> Result<U256, MockErr> {
            let mut inner = self.inner.lock().unwrap();
            inner.reads += 1;
            Ok(inner
                .storage
                .get(&(address, index))
                .copied()
                .unwrap_or_default())
        }
        fn block_hash(&self, _number: u64) -> Result<H256, MockErr> {
            Ok(H256::ZERO)
        }
    }

    impl BlockContext for MockDB {
        type Error = MockErr;
        fn block_info_arc(&self) -> Result<Arc<BlockInfo>, MockErr> {
            Ok(self
                .inner
                .lock()
                .unwrap()
                .last_block
                .clone()
                .unwrap_or_default())
        }
        fn state_diff_arc(&self) -> Result<Arc<BlockStorageDiff>, MockErr> {
            Ok(Arc::new(BlockStorageDiff::default()))
        }
    }

    impl EvmStorageWrite for MockDB {
        type Error = MockErr;
        fn update_block(
            &self,
            block_info: BlockInfo,
            block_diff: BlockStorageDiff,
        ) -> Result<(), MockErr> {
            let mut inner = self.inner.lock().unwrap();
            for account_diff in block_diff.storage_diffs.iter() {
                for iv in account_diff.diffs.iter() {
                    inner
                        .storage
                        .insert((account_diff.address, iv.index), iv.value);
                }
            }
            for account in block_diff.new_accounts.iter() {
                inner
                    .accounts
                    .insert(account.address, Some(account.clone().into()));
            }
            inner.last_block = Some(Arc::new(block_info));
            Ok(())
        }
        fn last_committed_block(&self) -> Result<Option<BlockInfo>, MockErr> {
            Ok(self
                .inner
                .lock()
                .unwrap()
                .last_block
                .as_ref()
                .map(|b| b.as_ref().clone()))
        }
    }

    impl BlockIndex for MockDB {
        type Error = MockErr;
        fn get_block_by_id_arc(
            &self,
            _block_id: BlockId,
        ) -> Result<Option<Arc<BlockInfo>>, MockErr> {
            Ok(self.inner.lock().unwrap().last_block.clone())
        }
    }

    fn key(n: u64) -> H256 {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&n.to_be_bytes());
        H256::from(b)
    }

    /// A chain of `depth` diff layers; layer `n` writes (addr, slot(n)) = n+1
    /// and account(n) with nonce n+1.
    fn build_chain(depth: u64) -> (Arc<LinkedDiffLayer>, H256) {
        let addr = key(0xabcd);
        let cache = Arc::new(LinkedDiffLayer::CacheDiskLayer(CacheDiskLayer::new(
            1000, 1000, 1000, 0, true,
        )));
        let mut prev = cache;
        for n in 0..depth {
            let mut diff = BlockStorageDiff::default();
            diff.storage_diffs.push(AccountStorageDiff {
                address: addr,
                diffs: vec![IndexValuePair {
                    index: key(1000 + n),
                    value: U256::from(n + 1),
                }],
            });
            diff.new_accounts.push(NewAccount {
                address: key(2000 + n),
                balance: U256::from(n + 1),
                nonce: n + 1,
                code_hash: H256::ZERO,
            });
            let mut info = BlockInfo::default();
            info.inner.header.hash = key(3000 + n);
            info.inner.header.inner.parent_hash = if n == 0 {
                H256::ZERO
            } else {
                key(3000 + n - 1)
            };
            info.inner.header.inner.number = n + 1;
            prev = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(info, diff, prev)));
        }
        (prev, addr)
    }

    #[test]
    fn reads_resolve_from_correct_layer() {
        let (top, addr) = build_chain(4);
        let db = HybridStateDB::new(top, MockDB::default(), Some(0));

        for n in 0..4u64 {
            assert_eq!(db.storage(addr, key(1000 + n)).unwrap(), U256::from(n + 1));
            assert_eq!(db.basic(key(2000 + n)).unwrap().unwrap().nonce, n + 1);
        }
        // Absent key falls through to the bottom DB (zero).
        assert_eq!(db.storage(addr, key(9999)).unwrap(), U256::ZERO);
        assert!(db.basic(key(9999)).unwrap().is_none());
        // Block hashes resolve from layer metadata.
        assert_eq!(db.block_hash(3).unwrap(), key(3000 + 2));
    }

    /// Batched reads resolve through the same layers as the scalar
    /// reads, only fetch each unique unresolved key once, and refill
    /// the shared cache so later reads skip the bottom DB entirely.
    #[test]
    fn batched_reads_match_scalar_and_dedup() {
        let (top, addr) = build_chain(4);
        let mock = MockDB::default();
        {
            let mut inner = mock.inner.lock().unwrap();
            inner.storage.insert((addr, key(7000)), U256::from(70u64));
            let mut info = AccountInfo::default();
            info.nonce = 9;
            inner.accounts.insert(key(7001), Some(info));
        }
        let db = HybridStateDB::new(top, mock.clone(), Some(0));

        // diff-layer hit, bottom hit, missing key and a duplicate.
        let storage_keys = vec![
            (addr, key(1000)),
            (addr, key(7000)),
            (addr, key(9999)),
            (addr, key(7000)),
        ];
        let batched = db.storage_many(&storage_keys).unwrap();
        assert_eq!(batched[0], U256::from(1u64));
        assert_eq!(batched[1], U256::from(70u64));
        assert_eq!(batched[2], U256::ZERO);
        assert_eq!(batched[3], U256::from(70u64));
        // 7000 deduped to one bottom read, 9999 read once.
        assert_eq!(mock.reads(), 2);

        let addresses = vec![key(2000), key(7001), key(8888), key(7001)];
        let accounts = db.basic_many(&addresses).unwrap();
        assert_eq!(accounts[0].as_ref().unwrap().nonce, 1);
        assert_eq!(accounts[1].as_ref().unwrap().nonce, 9);
        assert!(accounts[2].is_none());
        assert_eq!(accounts[3].as_ref().unwrap().nonce, 9);
        assert_eq!(mock.reads(), 4);

        // Scalar reads agree and are now served from diff layers/cache.
        for (k, got) in storage_keys.iter().zip(&batched) {
            assert_eq!(*got, db.storage(k.0, k.1).unwrap());
        }
        for (a, got) in addresses.iter().zip(&accounts) {
            assert_eq!(*got, db.basic(*a).unwrap());
        }
        // The batch refilled the shared cache: no further bottom reads.
        let _ = db.storage_many(&storage_keys).unwrap();
        let _ = db.basic_many(&addresses).unwrap();
        assert_eq!(mock.reads(), 4);
    }

    /// With the shared cache disabled the batch goes straight to the
    /// bottom DB for unresolved keys and never touches moka.
    #[test]
    fn batched_reads_bypass_disabled_cache() {
        let addr = key(0xabcd);
        let cache = Arc::new(LinkedDiffLayer::CacheDiskLayer(CacheDiskLayer::new(
            1000, 1000, 1000, 0, false,
        )));
        let mock = MockDB::default();
        mock.inner
            .lock()
            .unwrap()
            .storage
            .insert((addr, key(1)), U256::from(5u64));
        let db = HybridStateDB::new(cache, mock.clone(), None);

        let keys = vec![(addr, key(1)), (addr, key(1))];
        assert_eq!(
            db.storage_many(&keys).unwrap(),
            vec![U256::from(5u64), U256::from(5u64)]
        );
        // Deduped fetch, and repeated batches keep hitting the bottom DB.
        assert_eq!(mock.reads(), 1);
        let _ = db.storage_many(&keys).unwrap();
        assert_eq!(mock.reads(), 2);
    }

    #[test]
    fn handles_share_prebuilt_flattened_view() {
        let (top, addr) = build_chain(4);
        let flattened = FlattenedLayerView::build(top.clone());
        let first = HybridStateDB::from_flattened(flattened.clone(), MockDB::default(), Some(0));
        let second = HybridStateDB::from_flattened(flattened.clone(), MockDB::default(), Some(0));

        assert!(Arc::ptr_eq(&first.flattened, &flattened));
        assert!(Arc::ptr_eq(&second.flattened, &flattened));
        assert!(Arc::ptr_eq(flattened.head(), &top));
        assert_eq!(flattened.diff_layer_count(), 4);
        assert_eq!(first.storage(addr, key(1000)).unwrap(), U256::from(1));
        assert_eq!(second.storage(addr, key(1003)).unwrap(), U256::from(4));

        let empty_a = FlattenedLayerView::empty();
        let empty_b = FlattenedLayerView::empty();
        assert!(Arc::ptr_eq(&empty_a, &empty_b));
        assert_eq!(empty_a.diff_layer_count(), 0);
        assert!(matches!(empty_a.head().as_ref(), LinkedDiffLayer::Empty));

        let cache = Arc::new(LinkedDiffLayer::CacheDiskLayer(CacheDiskLayer::new(
            1, 1, 1, 0, true,
        )));
        let cache_view = FlattenedLayerView::build(cache.clone());
        assert!(Arc::ptr_eq(cache_view.head(), &cache));
    }

    #[test]
    fn handle_stays_consistent_across_commit() {
        let (top, addr) = build_chain(4);
        let mock = MockDB::default();
        let before = HybridStateDB::new(top.clone(), mock.clone(), Some(0));

        // Committing the two oldest layers retargets the chain.
        let bottom = top
            .clone()
            .cap_diff_to_db(2, mock.clone())
            .expect("commit failed");
        assert_eq!(bottom, 2);

        // The pre-commit handle keeps returning identical values.
        for n in 0..4u64 {
            assert_eq!(
                before.storage(addr, key(1000 + n)).unwrap(),
                U256::from(n + 1)
            );
            assert_eq!(before.basic(key(2000 + n)).unwrap().unwrap().nonce, n + 1);
        }

        // A post-commit handle reads committed keys through the cache/db.
        let after = HybridStateDB::new(top, mock, Some(2));
        for n in 0..4u64 {
            assert_eq!(
                after.storage(addr, key(1000 + n)).unwrap(),
                U256::from(n + 1)
            );
            assert_eq!(after.basic(key(2000 + n)).unwrap().unwrap().nonce, n + 1);
        }
    }
}

impl<StateDB: BlockContext> BlockContext for HybridStateDB<StateDB> {
    type Error = Error<StateDB::Error>;

    fn block_info_arc(&self) -> Result<Arc<BlockInfo>, Self::Error> {
        match self.flattened.head().as_ref() {
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
        match self.flattened.head().as_ref() {
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
