//! Read-path benchmark for the layered state (HybridStateDB over a
//! DiffLayer chain), mirroring the production standalone setup where
//! `--diff-depth-limit` keeps up to 256 in-memory diff layers and every
//! EVM state read walks the chain before reaching the bottom cache.
//!
//! Run: cargo bench -p leafage-evm-storage --bench layer_read

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use leafage_evm_storage::{CacheDiskLayer, DiffLayer, HybridStateDB, LinkedDiffLayer, StateDB};
use leafage_evm_types::{
    AccountInfo, AccountStorageDiff, BlockInfo, BlockStorageDiff, Bytecode, IndexValuePair,
    NewAccount, H256, U256,
};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, thiserror::Error)]
#[error("mock db error")]
struct MockErr;
impl revm::database_interface::DBErrorMarker for MockErr {}

/// Bottom "disk" returning constant values instantly, so the benchmark
/// isolates the in-memory walk instead of RocksDB.
#[derive(Debug, Clone)]
struct DiskMock;

impl StateDB for DiskMock {
    type Error = MockErr;
    fn basic(&self, _address: H256) -> Result<Option<AccountInfo>, MockErr> {
        Ok(Some(AccountInfo::default()))
    }
    fn code_by_hash(&self, _code_hash: H256) -> Result<Bytecode, MockErr> {
        Ok(Bytecode::default())
    }
    fn storage(&self, _address: H256, _index: H256) -> Result<U256, MockErr> {
        Ok(U256::from(1u64))
    }
    fn block_hash(&self, _number: u64) -> Result<H256, MockErr> {
        Ok(H256::ZERO)
    }
}

/// Deterministic pseudo-random H256 (splitmix64 stream).
fn h256(seed: u64) -> H256 {
    let mut out = [0u8; 32];
    let mut x = seed;
    for chunk in out.chunks_mut(8) {
        x = x.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_le_bytes());
    }
    H256::from(out)
}

const SLOTS_PER_BLOCK: usize = 2000;
const ACCOUNTS_PER_BLOCK: usize = 1200;

// Disjoint seed namespaces so probe keys never collide with layer keys.
const NS_ACCOUNT: u64 = 1 << 60;
const NS_CONTRACT: u64 = 2 << 60;
const NS_SLOT: u64 = 3 << 60;
const NS_HASH: u64 = 4 << 60;
const NS_PROBE: u64 = 5 << 60;

fn layer_contract(n: usize) -> H256 {
    h256(NS_CONTRACT | n as u64)
}

fn layer_slot(n: usize, s: usize) -> H256 {
    h256(NS_SLOT | (n * SLOTS_PER_BLOCK + s) as u64)
}

/// Build a chain shaped like production: `depth` diff layers on top of
/// a CacheDiskLayer, each block touching one contract with SLOTS_PER_BLOCK
/// slots and ACCOUNTS_PER_BLOCK accounts.
fn build_chain_with_cache(depth: usize, cache_enabled: bool) -> Arc<LinkedDiffLayer> {
    let cache = Arc::new(LinkedDiffLayer::CacheDiskLayer(CacheDiskLayer::new(
        200_000,
        2_000_000,
        20_000,
        0,
        cache_enabled,
    )));
    let mut prev = cache;
    for n in 0..depth {
        let mut diff = BlockStorageDiff::default();
        for a in 0..ACCOUNTS_PER_BLOCK {
            diff.new_accounts.push(NewAccount {
                address: h256(NS_ACCOUNT | ((n as u64) << 32) | a as u64),
                balance: U256::from(1u64),
                nonce: 1,
                code_hash: H256::ZERO,
            });
        }
        diff.storage_diffs.push(AccountStorageDiff {
            address: layer_contract(n),
            diffs: (0..SLOTS_PER_BLOCK)
                .map(|s| IndexValuePair {
                    index: layer_slot(n, s),
                    value: U256::from(7u64),
                })
                .collect(),
        });
        let mut info = BlockInfo::default();
        info.inner.header.hash = h256(NS_HASH | n as u64);
        info.inner.header.inner.number = n as u64 + 1;
        prev = Arc::new(LinkedDiffLayer::DiffLayer(DiffLayer::new(info, diff, prev)));
    }
    prev
}

fn build_chain(depth: usize) -> Arc<LinkedDiffLayer> {
    build_chain_with_cache(depth, true)
}

type Handle = HybridStateDB<DiskMock>;

fn make_handle(top: &Arc<LinkedDiffLayer>, depth: usize) -> Handle {
    HybridStateDB::new(top.clone(), DiskMock, Some(depth as u64))
}

/// The pre-flattening read path, kept in the benchmark so both sides of
/// the comparison use the current ahash-backed DiffLayer maps. Its
/// construction is O(1), while each miss follows `next` and takes one
/// RwLock read per traversed layer.
struct LinkedHandle {
    memory_layer: Arc<LinkedDiffLayer>,
    statedb: DiskMock,
}

impl LinkedHandle {
    fn new(memory_layer: Arc<LinkedDiffLayer>) -> Self {
        Self {
            memory_layer,
            statedb: DiskMock,
        }
    }

    fn storage(&self, address: H256, index: H256) -> Result<U256, MockErr> {
        Self::storage_from_layer(&self.memory_layer, address, index, &self.statedb)
    }

    #[inline]
    fn storage_from_layer(
        layer: &Arc<LinkedDiffLayer>,
        address: H256,
        index: H256,
        statedb: &DiskMock,
    ) -> Result<U256, MockErr> {
        match layer.as_ref() {
            LinkedDiffLayer::DiffLayer(diff) => match diff.storage.get(&(address, index)) {
                Some(value) => Ok(*value),
                None => {
                    let next = diff
                        .next
                        .read()
                        .expect("Failed to acquire read lock on diff layer");
                    Self::storage_from_layer(&next, address, index, statedb)
                }
            },
            // Request-lifecycle comparisons use a disabled cache so the
            // terminal operation is the same direct DB call on both paths.
            LinkedDiffLayer::CacheDiskLayer(_) | LinkedDiffLayer::Empty => {
                statedb.storage(address, index)
            }
        }
    }
}

const PROBE_KEYS: usize = 8192;

/// Storage keys absent from every diff layer; after one warm pass they
/// live in the bottom moka cache — the common production case (slot not
/// touched in the last `depth` blocks, hot in cache).
fn probe_keys() -> (H256, Vec<H256>) {
    let address = h256(NS_PROBE);
    let keys = (0..PROBE_KEYS as u64)
        .map(|i| h256(NS_PROBE | (i + 1)))
        .collect();
    (address, keys)
}

fn bench_walks(c: &mut Criterion) {
    for depth in [64usize, 256] {
        let top = build_chain(depth);
        let handle = make_handle(&top, depth);
        let (probe_addr, probes) = probe_keys();
        // Warm pass: refill puts the probe keys into the bottom cache.
        for k in &probes {
            handle.storage(probe_addr, *k).unwrap();
        }

        let mut group = c.benchmark_group(format!("depth{depth}"));
        group.throughput(Throughput::Elements(1));

        // Miss in all layers, hit in bottom cache (dominant case).
        let mut i = 0usize;
        group.bench_function(BenchmarkId::new("storage_miss_to_cache", depth), |b| {
            b.iter(|| {
                i = (i + 1) % PROBE_KEYS;
                handle.storage(probe_addr, probes[i]).unwrap()
            })
        });

        // Hit in the top layer (state written in the newest block).
        let top_addr = layer_contract(depth - 1);
        let mut s = 0usize;
        group.bench_function(BenchmarkId::new("storage_top_hit", depth), |b| {
            b.iter(|| {
                s = (s + 1) % SLOTS_PER_BLOCK;
                handle.storage(top_addr, layer_slot(depth - 1, s)).unwrap()
            })
        });

        // Hit half-way down the chain.
        let mid = depth / 2;
        let mid_addr = layer_contract(mid);
        let mut m = 0usize;
        group.bench_function(BenchmarkId::new("storage_mid_hit", depth), |b| {
            b.iter(|| {
                m = (m + 1) % SLOTS_PER_BLOCK;
                handle.storage(mid_addr, layer_slot(mid, m)).unwrap()
            })
        });

        // Account read missing all layers (falls through to cache).
        let mut a = 0usize;
        group.bench_function(BenchmarkId::new("account_miss_to_cache", depth), |b| {
            b.iter(|| {
                a = (a + 1) % PROBE_KEYS;
                handle.basic(probes[a]).unwrap()
            })
        });

        // Concurrent readers on one shared chain: 8 threads × 20k reads,
        // reproducing the per-layer lock traffic under RPC load.
        const THREADS: usize = 8;
        const READS: usize = 20_000;
        group.throughput(Throughput::Elements((THREADS * READS) as u64));
        group.bench_function(BenchmarkId::new("storage_miss_8threads", depth), |b| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let start = Instant::now();
                    std::thread::scope(|scope| {
                        for t in 0..THREADS {
                            let handle = make_handle(&top, depth);
                            let probes = &probes;
                            scope.spawn(move || {
                                let mut i = t;
                                for _ in 0..READS {
                                    i = (i + 1) % PROBE_KEYS;
                                    handle.storage(probe_addr, probes[i]).unwrap();
                                }
                            });
                        }
                    });
                    total += start.elapsed();
                }
                total
            })
        });

        group.finish();
    }
}

fn bench_request_case(
    c: &mut Criterion,
    name: &str,
    top: &Arc<LinkedDiffLayer>,
    address: H256,
    keys: &[H256],
    reads_per_request: &[usize],
) {
    let mut group = c.benchmark_group(name);

    for &reads in reads_per_request {
        group.throughput(Throughput::Elements(reads as u64));

        let mut linked_offset = 0usize;
        group.bench_with_input(BenchmarkId::new("linked", reads), &reads, |b, &reads| {
            b.iter(|| {
                let handle = LinkedHandle::new(top.clone());
                for n in 0..reads {
                    let key = keys[(linked_offset + n) % keys.len()];
                    black_box(handle.storage(address, key).unwrap());
                }
                linked_offset = (linked_offset + reads) % keys.len();
            });
        });

        let mut flat_offset = 0usize;
        group.bench_with_input(BenchmarkId::new("flat", reads), &reads, |b, &reads| {
            b.iter(|| {
                // Include the eager O(depth) flattening in every measured
                // request, matching StateTree::state_at's handle lifetime.
                let handle = HybridStateDB::new(top.clone(), DiskMock, None);
                for n in 0..reads {
                    let key = keys[(flat_offset + n) % keys.len()];
                    black_box(handle.storage(address, key).unwrap());
                }
                flat_offset = (flat_offset + reads) % keys.len();
            });
        });
    }

    group.finish();
}

/// Diff-layer handle lifecycle cost for the production Latest depth.
/// Unlike the steady-state benches above, every iteration constructs and
/// drops a fresh handle. The comparison isolates flattening from ahash:
/// both handles read the same current DiffLayer maps, and both use a direct
/// mock DB terminal. It intentionally does not model the rest of an RPC.
fn bench_request_lifecycle(c: &mut Criterion) {
    const DEPTH: usize = 256;
    let top = build_chain_with_cache(DEPTH, false);

    let mut handle_lifecycle = c.benchmark_group("request_lifecycle/depth256/handle_lifecycle");
    handle_lifecycle.throughput(Throughput::Elements(1));
    handle_lifecycle.bench_function("linked", |b| {
        b.iter(|| black_box(LinkedHandle::new(top.clone())));
    });
    handle_lifecycle.bench_function("flat", |b| {
        b.iter(|| black_box(HybridStateDB::new(top.clone(), DiskMock, None)));
    });
    handle_lifecycle.finish();

    let top_address = layer_contract(DEPTH - 1);
    let top_keys: Vec<_> = (0..SLOTS_PER_BLOCK)
        .map(|slot| layer_slot(DEPTH - 1, slot))
        .collect();
    bench_request_case(
        c,
        "request_lifecycle/depth256/top_hit",
        &top,
        top_address,
        &top_keys,
        &[1, 16, 256],
    );

    let middle = DEPTH / 2;
    let middle_address = layer_contract(middle);
    let middle_keys: Vec<_> = (0..SLOTS_PER_BLOCK)
        .map(|slot| layer_slot(middle, slot))
        .collect();
    bench_request_case(
        c,
        "request_lifecycle/depth256/mid_hit",
        &top,
        middle_address,
        &middle_keys,
        &[1, 2, 4, 16],
    );

    let (probe_address, probes) = probe_keys();
    bench_request_case(
        c,
        "request_lifecycle/depth256/miss_to_db",
        &top,
        probe_address,
        &probes,
        &[1, 2, 4, 16],
    );
}

criterion_group!(benches, bench_walks, bench_request_lifecycle);
criterion_main!(benches);
