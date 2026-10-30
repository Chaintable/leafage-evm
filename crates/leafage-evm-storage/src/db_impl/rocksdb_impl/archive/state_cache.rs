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
//! next version from above. A key that was never written is probed too.
//!
//! A probe that cannot extend the span is skipped: when the read is at the
//! head (the read alone reaches it), and, on v3, when a version above the one
//! just read is already known, from an earlier probe or read. The archive only
//! appends, so that newer version stays and the probe would only find it
//! again. A backfill reads a changing key at ever higher blocks, each time an
//! older version than the head's: only its first miss probes.
//!
//! **Staying correct.** The archive only appends final blocks above its
//! head: reorgs are resolved in the in-memory diff layers, and rewind is an
//! offline command. An append never invalidates a span that ends at or below
//! the head, so nothing is ever dropped. A reader takes the head before
//! creating its iterators, so they see every block up to it; spans are cut at
//! that head, and a reader above it does not use the cache.

use leafage_evm_types::{H256, U256};
use quick_cache::sync::{Cache as QuickCache, DefaultLifecycle, EntryAction, EntryResult};
use quick_cache::Weighter;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
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

/// Per-entry memory beyond the key and the spans: the `Arc<Entry>`, the span
/// vector's allocation and the cache's slot and hash table index. Measured:
/// 1M single-span storage entries take ~225 B each in RSS (64 B key + 56 B
/// span + ~105 B); moka took ~410 B.
const ENTRY_OVERHEAD: usize = 105;

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

    /// Whether both spans are of one version: the same block wrote them, or
    /// neither was ever written.
    fn same_version(&self, other: &Self) -> bool {
        self.found == other.found && self.lo == other.lo
    }
}

/// What a miss does about the key's next version, once it has read the
/// version it serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Probe {
    /// Look up the next version.
    Yes,
    /// Do not: the key is new to the cache, or changes too often.
    No,
    /// Not needed: the read is at the head, so its span reaches the head.
    AtHead,
    /// Not useful: a version above the one read is known, so the span cannot
    /// reach the head.
    Capped,
}

/// [`Entry::newest`] of a key not to probe again.
const VOLATILE: u64 = u64::MAX;

#[derive(Debug)]
pub(super) struct Entry<V> {
    /// The highest version of the key known to exist (0: none), or
    /// [`VOLATILE`]: probing found a short span, do not probe the key again.
    /// Only schema v3 records versions here.
    newest: u64,
    spans: Vec<Span<V>>,
}

impl<V> Entry<V> {
    /// Whether a version above the one `read` found is known to exist.
    fn knows_newer_than(&self, read: &Span<V>) -> bool {
        if read.found {
            self.newest > read.lo
        } else {
            self.newest > 0
        }
    }
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

/// Weighs an entry by the memory it takes.
#[derive(Clone, Copy)]
struct EntryWeighter;

impl<const N: usize, V> Weighter<Key<N>, Arc<Entry<V>>> for EntryWeighter {
    fn weight(&self, _: &Key<N>, entry: &Arc<Entry<V>>) -> u64 {
        entry_weight::<N, V>(entry.spans.capacity())
    }
}

fn entry_weight<const N: usize, V>(spans: usize) -> u64 {
    (N + ENTRY_OVERHEAD + spans * std::mem::size_of::<Span<V>>()) as u64
}

type SpanCache<const N: usize, V> = QuickCache<
    Key<N>,
    Arc<Entry<V>>,
    EntryWeighter,
    BuildHasherDefault<KeyHasher>,
    DefaultLifecycle<Key<N>, Arc<Entry<V>>>,
>;

/// The spans of `N`-byte keys holding `V` values.
pub(super) struct Table<const N: usize, V> {
    cache: SpanCache<N, V>,
}

impl<const N: usize, V> Table<N, V>
where
    V: Copy + PartialEq + std::fmt::Debug + Send + Sync + 'static,
{
    fn new(capacity_bytes: u64) -> Self {
        // Sized for single-span entries, the common case.
        let entries = capacity_bytes / entry_weight::<N, V>(1);
        Self {
            cache: SpanCache::with(
                entries.try_into().unwrap_or(usize::MAX),
                capacity_bytes,
                EntryWeighter,
                BuildHasherDefault::default(),
                DefaultLifecycle::default(),
            ),
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

    /// Fold a span just read into the key's entry. `next_version_found`:
    /// the span ends right before the key's next version (rather than at the
    /// read block or the head).
    pub(super) fn record(&self, key: [u8; N], span: Span<V>, next_version_found: bool) {
        // When the next version ended the span, it sits at `hi + 1`.
        let short = next_version_found && span.hi + 1 - span.lo < VOLATILE_SPAN;
        self.fold(key, span, short, 0);
    }

    /// Schema v3 [`Self::record`]: `newer` is a version above the span,
    /// found by probing at the head. It bounds the key's next version from
    /// above.
    pub(super) fn record_udt(&self, key: [u8; N], span: Span<V>, newer: Option<u64>) {
        let short = newer.is_some_and(|n| n - span.lo < VOLATILE_SPAN);
        let read = if span.found { span.lo } else { 0 };
        self.fold(key, span, short, newer.unwrap_or(0).max(read));
    }

    /// Merge `span` into the key's current entry under the shard lock, so
    /// concurrent misses on one key do not drop each other's spans. If
    /// another reader is inserting the key right now, skip.
    fn fold(&self, key: [u8; N], span: Span<V>, short: bool, newest: u64) {
        let key = Key(key);
        let mut grown = None;
        let result = self.cache.entry(&key, Some(Duration::ZERO), |_, entry| {
            let merged = Arc::new(merge(Some(&**entry), span, short, newest));
            if merged.spans.capacity() > entry.spans.capacity() {
                // A heavier entry goes back in through an insert: changing it
                // in place would not evict to make room for it.
                grown = Some(merged);
                EntryAction::ReplaceWithGuard
            } else {
                *entry = merged;
                EntryAction::Retain(())
            }
        });
        match result {
            EntryResult::Vacant(guard) => {
                let _ = guard.insert(Arc::new(merge(None, span, short, newest)));
            }
            EntryResult::Replaced(guard, _) => {
                let _ = guard.insert(grown.expect("set with ReplaceWithGuard"));
            }
            EntryResult::Retained(()) | EntryResult::Removed(..) | EntryResult::Timeout => {}
        }
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

    /// Whether a miss at `block` on a key with entry `prev`, which read the
    /// version `read` (only `found` and `lo` matter), should probe for the
    /// key's next version. It probes a key read before that does not change
    /// too often, unless that cannot extend the span.
    pub(super) fn probe<V>(
        prev: &Option<Arc<Entry<V>>>,
        view: &ReadView,
        block: u64,
        read: &Span<V>,
    ) -> Probe {
        match prev {
            Some(entry) if entry.newest != VOLATILE => {
                if block >= view.head {
                    Probe::AtHead
                } else if entry.knows_newer_than(read) {
                    Probe::Capped
                } else {
                    Probe::Yes
                }
            }
            _ => Probe::No,
        }
    }
}

fn merge<V: Copy + PartialEq + std::fmt::Debug>(
    prev: Option<&Entry<V>>,
    span: Span<V>,
    short: bool,
    newest: u64,
) -> Entry<V> {
    let kept: &[Span<V>] = prev.map_or(&[], |e| &e.spans);
    // Spans of different versions never overlap, and two reads that found
    // the same version saw the same value, so a span either extends one
    // already kept or is new.
    let extends = kept.iter().any(|s| s.same_version(&span));
    // Allocate exactly what is kept: the cache weighs entries by capacity,
    // and a first push would reserve room for four spans.
    let len = if extends {
        kept.len()
    } else {
        (kept.len() + 1).min(MAX_SPANS)
    };
    let mut spans: Vec<Span<V>> = Vec::with_capacity(len);
    spans.extend_from_slice(kept);
    if let Some(same) = spans.iter_mut().find(|s| s.same_version(&span)) {
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
    // `VOLATILE` is the greatest, so it stays.
    let newest = if short {
        VOLATILE
    } else {
        prev.map_or(0, |e| e.newest).max(newest)
    };
    Entry { newest, spans }
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

    fn view(head: u64) -> ReadView {
        ReadView { head }
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
        miss(cache, k, block);
        cache.storage.record(key(k), span, probed);
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
            let read = found(lo, 150, 2);
            assert_eq!(
                StateCache::probe(&prev, &view(30_000), 150, &read),
                Probe::Yes
            );
            cache
                .storage
                .record(key(k), found(lo, hi, 2), next_version_found);
            let read = found(19_000, 20_000, 3);
            StateCache::probe(&miss(&cache, k, 20_000), &view(30_000), 20_000, &read) == Probe::Yes
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
        assert_eq!(
            StateCache::probe(&None, &view(30_000), 150, &found(140, 150, 1)),
            Probe::No
        );
    }

    #[test]
    fn probe_skipped_when_it_cannot_extend_the_span() {
        let cache = StateCache::new(1 << 20);
        let probe = |k: u8, block: u64, head: u64, read: Span<U256>| {
            StateCache::probe(&miss(&cache, k, block), &view(head), block, &read)
        };
        cache.storage.record_udt(key(9), found(10, 15, 1), None);
        assert_eq!(probe(9, 16, 1000, found(10, 16, 1)), Probe::Yes);
        // A read at the head reaches the head without probing.
        assert_eq!(probe(9, 16, 16, found(10, 16, 1)), Probe::AtHead);
        // The probe finds version 500 above it: the span stays [10, 16], and
        // no version below 500 is probed again, nor the key's absence.
        cache
            .storage
            .record_udt(key(9), found(10, 16, 1), Some(500));
        assert_eq!(get(&cache, 9, 16), Some(found(10, 16, 1)));
        assert_eq!(probe(9, 17, 1000, found(10, 17, 1)), Probe::Capped);
        assert_eq!(probe(9, 400, 1000, found(300, 400, 2)), Probe::Capped);
        let absent = Span {
            lo: 0,
            hi: 5,
            found: false,
            value: U256::ZERO,
        };
        assert_eq!(probe(9, 5, 1000, absent), Probe::Capped);
        // Version 500 itself may still hold up to the head.
        assert_eq!(probe(9, 600, 1000, found(500, 600, 3)), Probe::Yes);

        // A read alone proves its version exists: older ones are not probed.
        cache.storage.record_udt(key(10), found(200, 250, 1), None);
        assert_eq!(probe(10, 150, 1000, found(100, 150, 2)), Probe::Capped);

        // A key never written is probed, and extended up to the head.
        cache
            .storage
            .record_udt(key(11), Span { hi: 20, ..absent }, None);
        assert_eq!(probe(11, 21, 1000, Span { hi: 21, ..absent }), Probe::Yes);
        cache
            .storage
            .record_udt(key(11), Span { hi: 1000, ..absent }, None);
        assert_eq!(get(&cache, 11, 1000), Some(Span { hi: 1000, ..absent }));

        // v2 learns the exact next version, so its reads never cap a key.
        record(&cache, 12, 150, found(100, 150, 1), false);
        record(&cache, 12, 250, found(200, 250, 2), false);
        assert_eq!(probe(12, 160, 1000, found(100, 160, 1)), Probe::Yes);
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

    #[test]
    fn concurrent_misses_keep_both_spans() {
        let cache = StateCache::new(1 << 20);
        // Two readers miss the same key, then record different versions.
        miss(&cache, 9, 15);
        miss(&cache, 9, 105);
        cache.storage.record(key(9), found(10, 15, 1), false);
        cache.storage.record(key(9), found(100, 105, 2), false);
        assert_eq!(get(&cache, 9, 12).map(|s| s.value), Some(U256::from(1)));
        assert_eq!(get(&cache, 9, 102).map(|s| s.value), Some(U256::from(2)));
    }

    #[test]
    fn growing_entries_stay_within_the_budget() {
        let cache = StateCache::new(1 << 20);
        let capacity = cache.storage.cache.capacity();
        // Fill the storage table with single-span entries, then give each
        // resident one more spans, without inserting any new key.
        let fit = capacity / entry_weight::<64, U256>(1);
        let keys: Vec<[u8; 64]> = (0..fit).map(bench_key).collect();
        for k in &keys {
            put(&cache, *k, found(10, 20, 7));
        }
        let keys: Vec<[u8; 64]> = keys
            .into_iter()
            .filter(|k| matches!(cache.storage.lookup(k, 15), Lookup::Hit(_)))
            .collect();
        assert!(keys.len() as u64 > fit / 2, "{} resident", keys.len());
        for i in 1..MAX_SPANS as u64 {
            for k in &keys {
                put(&cache, *k, found(i * 100, i * 100 + 5, 7));
            }
        }
        assert!(
            cache.storage.cache.weight() <= capacity,
            "{} > {capacity}",
            cache.storage.cache.weight()
        );
    }

    /// Uniform 64-byte keys, as keccak hashes are.
    fn bench_key(i: u64) -> [u8; 64] {
        let mut x = i.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut k = [0u8; 64];
        for word in k.chunks_mut(8) {
            x ^= x >> 30;
            x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
            x ^= x >> 27;
            x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
            x ^= x >> 31;
            word.copy_from_slice(&x.to_le_bytes());
        }
        k
    }

    fn rss_bytes() -> u64 {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
        let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
        pages * 4096
    }

    fn put(cache: &StateCache, k: [u8; 64], span: Span<U256>) {
        if let Lookup::Miss(_) = cache.storage.lookup(&k, span.hi) {
            cache.storage.record(k, span, false);
        }
    }

    /// Memory per entry, hit cost, and hit rate under a skewed workload.
    /// `cargo test --release -p leafage-evm-storage --lib bench_state_cache
    /// -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore]
    fn bench_state_cache() {
        const ENTRIES: u64 = 1_000_000;
        let keys: Vec<[u8; 64]> = (0..ENTRIES).map(bench_key).collect();

        // Memory: a budget just above the entries, as in production.
        let budget = ENTRIES * entry_weight::<64, U256>(1) * 4 / 3 * 11 / 10;
        let before = rss_bytes();
        let cache = StateCache::new(budget);
        for k in &keys {
            put(&cache, *k, found(10, 20, 7));
        }
        let after = rss_bytes();
        println!(
            "memory: {:.0} B/entry RSS, {} entries kept, weight {} B/entry",
            (after - before) as f64 / ENTRIES as f64,
            cache.storage.cache.len(),
            entry_weight::<64, U256>(1)
        );

        // Hit cost, one thread, random keys over the whole cache.
        let start = std::time::Instant::now();
        let mut hits = 0u64;
        for i in 0..ENTRIES {
            let k = &keys[(i.wrapping_mul(0x9e37_79b9) % ENTRIES) as usize];
            if matches!(cache.storage.lookup(k, 15), Lookup::Hit(_)) {
                hits += 1;
            }
        }
        assert!(hits > ENTRIES * 99 / 100, "{hits}");
        println!(
            "hit, 1 thread, random over 1M: {:.0} ns",
            start.elapsed().as_nanos() as f64 / ENTRIES as f64
        );

        // Hit cost, 8 threads on 1024 hot keys.
        let threads = 8u64;
        let per_thread = 1_000_000u64;
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            for t in 0..threads {
                let (cache, keys) = (&cache, &keys);
                scope.spawn(move || {
                    for i in 0..per_thread {
                        let k = &keys[((i * 7 + t * 131) % 1024) as usize];
                        assert!(matches!(cache.storage.lookup(k, 15), Lookup::Hit(_)));
                    }
                });
            }
        });
        println!(
            "hit, 8 threads on 1024 keys: {:.0} ns",
            start.elapsed().as_nanos() as f64 * threads as f64 / (threads * per_thread) as f64
        );
        drop(cache);

        // Hit rate: Zipf(0.9) over 1M keys, in the byte budget that holds
        // 100k storage entries under moka's weights (64 + 288 + 56 B each;
        // storage gets three quarters of the budget).
        let weights: Vec<f64> = (1..=ENTRIES).map(|r| 1.0 / (r as f64).powf(0.9)).collect();
        let total: f64 = weights.iter().sum();
        let mut cdf = Vec::with_capacity(weights.len());
        let mut acc = 0.0;
        for w in &weights {
            acc += w / total;
            cdf.push(acc);
        }
        let cache = StateCache::new(100_000 * 408 * 4 / 3);
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let (mut lookups, mut hits) = (0u64, 0u64);
        for round in 0..5_000_000u64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let u = (x >> 11) as f64 / (1u64 << 53) as f64;
            let rank = cdf.partition_point(|&c| c < u).min(ENTRIES as usize - 1);
            let k = keys[rank];
            if round >= 1_000_000 {
                lookups += 1;
            }
            match cache.storage.lookup(&k, 15) {
                Lookup::Hit(_) => {
                    if round >= 1_000_000 {
                        hits += 1;
                    }
                }
                Lookup::Miss(_) => cache.storage.record(k, found(10, 20, 7), false),
            }
        }
        println!(
            "hit rate, Zipf(0.9) over 1M keys, moka's 100k-entry budget: {:.1} %",
            100.0 * hits as f64 / lookups as f64
        );
    }
}
