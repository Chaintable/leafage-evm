//! Cross-request cache of archive account and storage reads.
//!
//! An archive read at block `n` returns the greatest version `v <= n` of a
//! key. That answer holds for every block in `[v, n]`, and up to the block
//! before the key's next version if that one is known. The cache keeps these
//! block spans per key, so a later read of the same key at any block inside a
//! span skips the RocksDB seek. Requests for one block and nearby blocks touch
//! the same contracts over and over, but the request-scoped `CacheDB` only
//! dedupes within one request.
//!
//! **Learning the whole span.** A seek alone only proves `[v, n]`, which a
//! later, higher block misses. When a key is read a second time and misses,
//! the reader also steps the iterator back to the key's next newer version
//! (versions sort newest-first) and caches the span up to it. That probe costs
//! about one more seek, so a key whose next version turns out fewer than
//! [`VOLATILE_SPAN`] blocks later is marked volatile and never probed again.
//! On schema v3 the probe is a `Get` at the head instead: the newest version
//! is the read one if the value holds up to the head, otherwise it bounds the
//! next version from above.
//!
//! **Staying correct.** The archive only appends final blocks above its
//! head: reorgs are resolved in the in-memory diff layers, and rewind is an
//! offline command. An append never invalidates a span that ends at or below
//! the head, so nothing is ever dropped. A reader takes the head before
//! creating its iterators, so they see every block up to it; spans are cut at
//! that head, and a reader above it does not use the cache.

use leafage_evm_types::{H256, U256};
use moka::sync::Cache as MokaCache;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::info;

/// Default weighted capacity (MB) of the cache.
const DEFAULT_STATE_CACHE_MB: u64 = 512;

/// Spans kept per key. Readers at different heights (a backfill far behind
/// the head next to one at the head) each keep their own span.
const MAX_SPANS: usize = 4;

/// A key whose probed next version is fewer than this many blocks after the
/// read version is volatile: it changes so often that probing costs more
/// than the hits it buys.
const VOLATILE_SPAN: u64 = 64;

/// Per-entry memory beyond the key and the spans: the `Arc<Entry>` and moka's
/// bookkeeping (hash table node, access-order node, key `Arc`, entry info).
/// Measured: 1M single-span storage entries take ~410 B each in RSS
/// (64 B key + 56 B span + ~290 B), accounts ~422 B (32 + 104 + ~286).
const ENTRY_OVERHEAD: usize = 288;

/// The account fields the archive stores, without the address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CachedAccount {
    pub balance: U256,
    pub nonce: u64,
    pub code_hash: H256,
}

/// One block span over which a key holds a single value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Span<V> {
    /// The version (block) that wrote the value, or 0 when no version exists.
    pub lo: u64,
    /// The last block the value is known to hold at.
    pub hi: u64,
    /// Whether a version exists at all (`false`: the key was never written
    /// at or below `hi`).
    pub found: bool,
    pub value: V,
}

impl<V> Span<V> {
    fn covers(&self, block: u64) -> bool {
        self.lo <= block && block <= self.hi
    }
}

#[derive(Debug)]
pub(super) struct Entry<V> {
    /// Probing found a short span; do not probe this key again.
    volatile: bool,
    spans: Vec<Span<V>>,
}

/// The archive head a [`super::StateDB`]'s iterators were created after:
/// every block up to it is visible to them.
#[derive(Clone, Copy, Debug)]
pub(super) struct ReadView {
    head: u64,
}

impl ReadView {
    pub(super) fn head(&self) -> u64 {
        self.head
    }
}

/// Lookup result. A miss returns the key's entry (if any) for
/// [`Table::record`] to merge into.
pub(super) enum Lookup<V> {
    Hit(Span<V>),
    Miss(Option<Arc<Entry<V>>>),
}

/// A cache key: an account's address hash, or a slot's address hash then
/// slot hash.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key<const N: usize>([u8; N]);

impl<const N: usize> Hash for Key<N> {
    /// Each 32-byte word is a keccak hash, so its first 8 bytes are already
    /// uniform: fold those rather than run SipHash over the whole key.
    fn hash<H: Hasher>(&self, state: &mut H) {
        let word = |at: usize| u64::from_le_bytes(self.0[at..at + 8].try_into().unwrap());
        state.write_u64((0..N).step_by(32).fold(0, |hash, at| hash ^ word(at)));
    }
}

/// Passes the `u64` a [`Key`] hashes to through unchanged.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _: &[u8]) {
        unreachable!("a Key hashes as one u64")
    }

    fn write_u64(&mut self, hash: u64) {
        self.0 = hash;
    }
}

/// The spans of `N`-byte keys holding `V` values.
pub(super) struct Table<const N: usize, V> {
    cache: MokaCache<Key<N>, Arc<Entry<V>>, BuildHasherDefault<KeyHasher>>,
}

impl<const N: usize, V> Table<N, V>
where
    V: Copy + PartialEq + std::fmt::Debug + Send + Sync + 'static,
{
    fn new(capacity_bytes: u64) -> Self {
        Self {
            cache: MokaCache::builder()
                .max_capacity(capacity_bytes)
                .weigher(|_: &Key<N>, entry: &Arc<Entry<V>>| {
                    let spans = entry.spans.capacity() * std::mem::size_of::<Span<V>>();
                    (N + ENTRY_OVERHEAD + spans).try_into().unwrap_or(u32::MAX)
                })
                .build_with_hasher(BuildHasherDefault::default()),
        }
    }

    pub(super) fn lookup(&self, key: &[u8; N], block: u64) -> Lookup<V> {
        let entry = self.cache.get(&Key(*key));
        if let Some(span) = entry
            .as_ref()
            .and_then(|e| e.spans.iter().find(|s| s.covers(block)))
        {
            return Lookup::Hit(*span);
        }
        Lookup::Miss(entry)
    }

    /// Fold a span just read into `prev`, the key's entry from the miss.
    /// `next_version_found`: the span ends right before the key's next
    /// version (rather than at the read block or the head).
    pub(super) fn record(
        &self,
        key: [u8; N],
        prev: Option<Arc<Entry<V>>>,
        span: Span<V>,
        next_version_found: bool,
    ) {
        // When the next version ended the span, it sits at `hi + 1`.
        let short = next_version_found && span.hi + 1 - span.lo < VOLATILE_SPAN;
        self.cache
            .insert(Key(key), Arc::new(merge(prev.as_deref(), span, short)));
    }

    /// Schema v3 [`Self::record`]: `newer` is a version above the span,
    /// found by probing at the head. It bounds the key's next version from
    /// above.
    pub(super) fn record_udt(
        &self,
        key: [u8; N],
        prev: Option<Arc<Entry<V>>>,
        span: Span<V>,
        newer: Option<u64>,
    ) {
        let short = newer.is_some_and(|n| n - span.lo < VOLATILE_SPAN);
        self.cache
            .insert(Key(key), Arc::new(merge(prev.as_deref(), span, short)));
    }
}

pub(super) struct StateCache {
    pub(super) accounts: Table<32, Option<CachedAccount>>,
    pub(super) storage: Table<64, U256>,
    /// Highest block committed so far.
    head: AtomicU64,
}

impl std::fmt::Debug for StateCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateCache")
            .field("head", &self.head.load(Ordering::Relaxed))
            .finish()
    }
}

impl StateCache {
    /// Build the cache honoring `ARCHIVE_STATE_CACHE_MB` (`0` disables).
    pub(super) fn from_env() -> Option<Self> {
        let mb = std::env::var("ARCHIVE_STATE_CACHE_MB")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_STATE_CACHE_MB);
        if mb == 0 {
            info!(target: "rocksdb", "archive state cache disabled");
            return None;
        }
        info!(target: "rocksdb", "archive state cache enabled: {}MB", mb);
        Some(Self::new(mb.saturating_mul(1024 * 1024)))
    }

    pub(super) fn new(capacity_bytes: u64) -> Self {
        // Accounts are fewer than slots; give them a quarter of the budget.
        let account_bytes = capacity_bytes / 4;
        Self {
            accounts: Table::new(account_bytes),
            storage: Table::new(capacity_bytes - account_bytes),
            head: AtomicU64::new(0),
        }
    }

    /// Raise the head to `block` once it is committed (and seed it when the
    /// archive is opened).
    pub(super) fn advance_head(&self, block: u64) {
        self.head.fetch_max(block, Ordering::SeqCst);
    }

    /// The view of a reader at `block`. Take it before creating the reader's
    /// iterators, so they see every block up to its head. `None` for a reader
    /// above the head: a span there could cover a block a later append writes.
    pub(super) fn view(&self, block: u64) -> Option<ReadView> {
        let head = self.head.load(Ordering::SeqCst);
        (block <= head).then_some(ReadView { head })
    }

    /// Whether a miss on a key with this entry should probe for its next
    /// version: the key was read before, and does not change too often.
    pub(super) fn should_probe<V>(prev: &Option<Arc<Entry<V>>>) -> bool {
        prev.as_ref().is_some_and(|e| !e.volatile)
    }
}

fn merge<V: Copy + PartialEq + std::fmt::Debug>(
    prev: Option<&Entry<V>>,
    span: Span<V>,
    short: bool,
) -> Entry<V> {
    let kept: &[Span<V>] = prev.map_or(&[], |e| &e.spans);
    // Spans of different versions never overlap, and two reads that found
    // the same version saw the same value, so a span either extends one
    // already kept or is new.
    let extends = kept
        .iter()
        .any(|s| s.found == span.found && s.lo == span.lo);
    // Allocate exactly what is kept: the cache weighs entries by capacity,
    // and a first push would reserve room for four spans.
    let len = if extends {
        kept.len()
    } else {
        (kept.len() + 1).min(MAX_SPANS)
    };
    let mut spans: Vec<Span<V>> = Vec::with_capacity(len);
    spans.extend_from_slice(kept);
    if let Some(same) = spans
        .iter_mut()
        .find(|s| s.found == span.found && s.lo == span.lo)
    {
        debug_assert_eq!(same.value, span.value);
        same.hi = same.hi.max(span.hi);
    } else {
        if spans.len() == MAX_SPANS {
            // Drop the span at the lowest heights.
            let (oldest, _) = spans
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| s.hi)
                .expect("MAX_SPANS > 0");
            spans.swap_remove(oldest);
        }
        spans.push(span);
    }
    let volatile = prev.is_some_and(|e| e.volatile) || short;
    Entry { volatile, spans }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::BuildHasher;

    fn found(lo: u64, hi: u64, value: u64) -> Span<U256> {
        Span {
            lo,
            hi,
            found: true,
            value: U256::from(value),
        }
    }

    fn key(b: u8) -> [u8; 64] {
        [b; 64]
    }

    fn get(cache: &StateCache, k: u8, block: u64) -> Option<Span<U256>> {
        match cache.storage.lookup(&key(k), block) {
            Lookup::Hit(span) => Some(span),
            Lookup::Miss(_) => None,
        }
    }

    fn miss(cache: &StateCache, k: u8, block: u64) -> Option<Arc<Entry<U256>>> {
        match cache.storage.lookup(&key(k), block) {
            Lookup::Hit(_) => panic!("expected a miss"),
            Lookup::Miss(prev) => prev,
        }
    }

    fn record(cache: &StateCache, k: u8, block: u64, span: Span<U256>, probed: bool) {
        let prev = miss(cache, k, block);
        cache.storage.record(key(k), prev, span, probed);
    }

    #[test]
    fn hits_only_inside_spans_and_extends_same_version() {
        let cache = StateCache::new(1 << 20);
        record(&cache, 1, 20, found(10, 20, 7), false);
        assert_eq!(get(&cache, 1, 10), Some(found(10, 20, 7)));
        assert_eq!(get(&cache, 1, 20).map(|s| s.value), Some(U256::from(7)));
        assert!(get(&cache, 1, 9).is_none());
        assert!(get(&cache, 1, 21).is_none());
        // A later read that found the same version extends the span.
        record(&cache, 1, 30, found(10, 30, 7), false);
        assert_eq!(get(&cache, 1, 25), Some(found(10, 30, 7)));
        // An older version gets its own span.
        record(&cache, 1, 5, found(3, 5, 1), false);
        assert_eq!(get(&cache, 1, 4).map(|s| s.value), Some(U256::from(1)));
        assert_eq!(get(&cache, 1, 30).map(|s| s.value), Some(U256::from(7)));
    }

    #[test]
    fn absent_key_spans_from_zero() {
        let cache = StateCache::new(1 << 20);
        let absent = Span {
            lo: 0,
            hi: 50,
            found: false,
            value: U256::ZERO,
        };
        record(&cache, 2, 50, absent, false);
        assert_eq!(get(&cache, 2, 0), Some(absent));
        assert!(get(&cache, 2, 51).is_none());
    }

    #[test]
    fn keeps_at_most_max_spans_dropping_lowest() {
        let cache = StateCache::new(1 << 20);
        for i in 0..=MAX_SPANS as u64 {
            record(&cache, 3, i * 10 + 5, found(i * 10, i * 10 + 5, i), false);
        }
        assert!(get(&cache, 3, 0).is_none(), "lowest span evicted");
        for i in 1..=MAX_SPANS as u64 {
            assert_eq!(get(&cache, 3, i * 10).map(|s| s.value), Some(U256::from(i)));
        }
    }

    #[test]
    fn short_probed_span_marks_key_volatile() {
        let cache = StateCache::new(1 << 20);
        // Key `k`, read once at [90, 100], misses at block 150 and probes:
        // the read found version `lo`, and its span ends at `hi`. Returns
        // whether the key is still probed afterwards.
        let probe = |k: u8, lo: u64, hi: u64, next_version_found: bool| {
            record(&cache, k, 100, found(90, 100, 1), false);
            let prev = miss(&cache, k, 150);
            assert!(StateCache::should_probe(&prev));
            cache
                .storage
                .record(key(k), prev, found(lo, hi, 2), next_version_found);
            StateCache::should_probe(&miss(&cache, k, 20_000))
        };
        // The next version 3 blocks later.
        assert!(!probe(4, 145, 147, true));
        // The next version just under and exactly `VOLATILE_SPAN` blocks later.
        assert!(!probe(5, 140, 140 + VOLATILE_SPAN - 2, true));
        assert!(probe(6, 140, 140 + VOLATILE_SPAN - 1, true));
        // A short span cut at the head (no newer version yet) says nothing
        // about how often the key changes.
        assert!(probe(7, 145, 151, false));
        // A key seen for the first time is not probed.
        assert!(!StateCache::should_probe::<U256>(&None));
    }

    #[test]
    fn readers_above_the_head_bypass_the_cache() {
        let cache = StateCache::new(1 << 20);
        cache.advance_head(100);
        assert_eq!(cache.view(100).map(|v| v.head()), Some(100));
        assert!(cache.view(101).is_none());
        cache.advance_head(101);
        assert!(cache.view(101).is_some());
    }

    #[test]
    fn spans_take_exactly_their_capacity() {
        let cache = StateCache::new(1 << 20);
        record(&cache, 8, 10, found(5, 10, 1), false);
        for i in 1..=MAX_SPANS as u64 + 2 {
            record(&cache, 8, i * 100, found(i * 100, i * 100, i), false);
        }
        record(&cache, 8, 650, found(600, 650, 6), false);
        let entry = miss(&cache, 8, 1).unwrap();
        assert_eq!(entry.spans.len(), MAX_SPANS);
        assert_eq!(entry.spans.capacity(), MAX_SPANS);
    }

    #[test]
    fn slot_keys_hash_both_the_address_and_the_slot() {
        let hash = |k: [u8; 64]| BuildHasherDefault::<KeyHasher>::default().hash_one(Key(k));
        let base = key(1);
        let mut other_slot = base;
        other_slot[32] ^= 1;
        let mut other_address = base;
        other_address[0] ^= 1;
        assert_ne!(hash(base), hash(other_slot));
        assert_ne!(hash(base), hash(other_address));
    }
}
