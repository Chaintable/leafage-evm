//! End-to-end synchronization scenarios for `StateTree`.
//!
//! This benchmark calls the public `StateTree` methods against a real temporary
//! RocksDB instead of measuring synchronization primitives in isolation. It is
//! compatible with both the legacy three-`RwLock` implementation and the
//! `ArcSwap` snapshot implementation so the same workload can run on both.

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use leafage_evm_storage::{
    BlockIndex, EvmStorageRead, EvmStorageWrite, MultiStorage, StateDBProvider, StateDBWrapper,
    StateTree, StateTreeConfig, StorageKind,
};
use leafage_evm_types::{BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff, H256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const INDEX_SIZE: u64 = 64;
const READERS: usize = 8;
const READS_PER_THREAD: usize = 10_000;
const WRITES_PER_MIXED_ROUND: usize = 32;

fn hash(number: u64) -> H256 {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&number.to_be_bytes());
    bytes[8..16].copy_from_slice(&number.rotate_left(17).to_be_bytes());
    H256::from(bytes)
}

fn block_info(number: u64) -> BlockInfo {
    let mut info = BlockInfo::default();
    info.inner.header.hash = hash(number);
    info.inner.header.inner.parent_hash = if number == 0 {
        H256::ZERO
    } else {
        hash(number - 1)
    };
    info.inner.header.inner.number = number;
    info
}

fn temp_path(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "leafage-state-tree-{label}-{}-{unique}",
        std::process::id()
    ))
}

struct Fixture {
    tree: Option<Arc<StateTree<MultiStorage>>>,
    path: PathBuf,
    next_block: AtomicU64,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let path = temp_path(label);
        let db = MultiStorage::open(&path, 64, StorageKind::Rocksdb, false, false, false).unwrap();
        StateDBWrapper(
            db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                .unwrap()
                .unwrap(),
        )
        .update_block(block_info(0), BlockStorageDiff::default())
        .unwrap();

        let tree = Arc::new(
            StateTree::new(
                db,
                StateTreeConfig::new(INDEX_SIZE as usize, 1_024, 1_024, 1_024, true),
            )
            .unwrap(),
        );
        for number in 1..=INDEX_SIZE {
            tree.update_block(block_info(number), BlockStorageDiff::default())
                .unwrap();
        }

        Self {
            tree: Some(tree),
            path,
            next_block: AtomicU64::new(INDEX_SIZE + 1),
        }
    }

    fn tree(&self) -> &Arc<StateTree<MultiStorage>> {
        self.tree.as_ref().unwrap()
    }

    fn publish_next(&self) -> u64 {
        let number = self.next_block.fetch_add(1, Ordering::Relaxed);
        self.tree()
            .update_block(block_info(number), BlockStorageDiff::default())
            .unwrap();
        number
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The tree owns RocksDB; release it before removing the benchmark DB.
        // At this point no reader threads remain.
        drop(self.tree.take());
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn query_for(iteration: usize) -> BlockId {
    let number = (iteration as u64 % INDEX_SIZE) + 1;
    match iteration & 3 {
        0 | 1 => BlockId::Number(BlockNumberOrTag::Latest),
        2 => BlockId::Hash(hash(number).into()),
        _ => BlockId::Number(BlockNumberOrTag::Number(number)),
    }
}

fn timed_parallel<F>(criterion_iters: u64, read: &F) -> Duration
where
    F: Fn(usize) -> u64 + Sync,
{
    let start = Instant::now();
    for _ in 0..criterion_iters {
        std::thread::scope(|scope| {
            for thread_id in 0..READERS {
                scope.spawn(move || {
                    let mut checksum = 0u64;
                    for i in 0..READS_PER_THREAD {
                        checksum ^= read(i + thread_id * 31);
                    }
                    black_box(checksum);
                });
            }
        });
    }
    start.elapsed()
}

fn bench_state_tree_reads(c: &mut Criterion) {
    let fixture = Fixture::new("reads");

    let mut block_index = c.benchmark_group("state_tree_block_index_mixed_8_threads");
    block_index.throughput(Throughput::Elements((READERS * READS_PER_THREAD) as u64));
    block_index.bench_function("latest50_hash25_number25", |b| {
        b.iter_custom(|iters| {
            timed_parallel(iters, &|i| {
                fixture
                    .tree()
                    .get_block_by_id_arc(query_for(i))
                    .unwrap()
                    .unwrap()
                    .header
                    .number
            })
        })
    });
    block_index.finish();

    let mut state_at = c.benchmark_group("state_tree_state_at_mixed_8_threads");
    state_at.throughput(Throughput::Elements((READERS * READS_PER_THREAD) as u64));
    state_at.bench_function("latest50_hash25_number25", |b| {
        b.iter_custom(|iters| {
            timed_parallel(iters, &|i| {
                let state = fixture.tree().state_at(query_for(i)).unwrap().unwrap();
                black_box(state);
                i as u64
            })
        })
    });
    state_at.finish();
}

fn bench_state_tree_sequential_update(c: &mut Criterion) {
    let fixture = Fixture::new("sequential-update");
    let mut group = c.benchmark_group("state_tree_update_block_sequential");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(3));
    group.bench_function("empty_diff_depth64", |b| {
        b.iter(|| black_box(fixture.publish_next()))
    });
    group.finish();
}

fn timed_mixed_rounds(criterion_iters: u64, fixture: &Fixture) -> Duration {
    let start = Instant::now();
    for _ in 0..criterion_iters {
        let barrier = Arc::new(Barrier::new(READERS + 1));
        std::thread::scope(|scope| {
            for _ in 0..READERS {
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    let mut checksum = 0u64;
                    for _ in 0..READS_PER_THREAD {
                        checksum ^= fixture
                            .tree()
                            .get_block_by_id_arc(BlockId::Number(BlockNumberOrTag::Latest))
                            .unwrap()
                            .unwrap()
                            .header
                            .number;
                    }
                    black_box(checksum);
                });
            }
            barrier.wait();
            for _ in 0..WRITES_PER_MIXED_ROUND {
                black_box(fixture.publish_next());
            }
        });
    }
    start.elapsed()
}

fn bench_state_tree_read_while_publishing(c: &mut Criterion) {
    let fixture = Fixture::new("mixed-publish");
    let mut group = c.benchmark_group("state_tree_latest_read_with_sequential_writer");
    group.throughput(Throughput::Elements((READERS * READS_PER_THREAD) as u64));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(3));
    group.bench_function("8_readers_32_writes", |b| {
        b.iter_custom(|iters| timed_mixed_rounds(iters, &fixture))
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_state_tree_reads,
    bench_state_tree_sequential_update,
    bench_state_tree_read_while_publishing
);
criterion_main!(benches);
