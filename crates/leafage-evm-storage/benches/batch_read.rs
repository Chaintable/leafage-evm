//! Batched vs scalar point reads on the non-archive RocksDB backend:
//! `read_storage`/`read_account` loops against the MultiGet-backed
//! `read_storage_many`/`read_account_many` overrides, over SST-resident
//! data (memtables are flushed after setup). Key sets rotate through a
//! 200k keyspace so consecutive iterations don't replay the exact same
//! blocks, but after warmup this still measures the warm block-cache
//! path — the conservative case for MultiGet, whose additional win on
//! cold data is parallel IO inside one call.
//!
//! Run: cargo bench -p leafage-evm-storage --bench batch_read

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use leafage_evm_storage::{EvmStorageWrite, RocksDBStorage, StateDBRead, StateDBWrapper};
use leafage_evm_types::{
    AccountStorageDiff, Block, BlockInfo, BlockStorageDiff, IndexValuePair, NewAccount, H256, U256,
};
use std::sync::Arc;

const KEYSPACE: u64 = 200_000;
const BLOCK_CHUNK: u64 = 20_000;

fn key(tag: u64, n: u64) -> H256 {
    let mut raw = [0u8; 32];
    raw[..8].copy_from_slice(&tag.to_be_bytes());
    raw[8..16].copy_from_slice(&n.to_be_bytes());
    // Spread the low bytes so keys are not lexicographically clustered
    // by insertion order (production keys are keccak-distributed).
    raw[24..32].copy_from_slice(&(n.wrapping_mul(0x9e3779b97f4a7c15)).to_be_bytes());
    H256::from(raw)
}

fn block_info(number: u64, hash: H256, parent_hash: H256) -> BlockInfo {
    let mut info = BlockInfo {
        inner: Block::empty(Default::default()),
        other: Default::default(),
    };
    info.inner.header.hash = hash;
    info.inner.header.inner.number = number;
    info.inner.header.inner.parent_hash = parent_hash;
    info
}

struct Fixture {
    db: Arc<RocksDBStorage>,
    dir: std::path::PathBuf,
    accounts: Vec<H256>,
    storage_keys: Vec<(H256, H256)>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup() -> Fixture {
    let dir = std::env::temp_dir().join(format!("leafage-bench-batch-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Arc::new(RocksDBStorage::open(&dir, 256, false));
    let state = StateDBWrapper(db.clone());

    let mut accounts = Vec::with_capacity(KEYSPACE as usize);
    let mut storage_keys = Vec::with_capacity(KEYSPACE as usize);
    let mut parent = H256::ZERO;
    for chunk_start in (0..KEYSPACE).step_by(BLOCK_CHUNK as usize) {
        let mut diff = BlockStorageDiff::default();
        for n in chunk_start..chunk_start + BLOCK_CHUNK {
            let address = key(1, n);
            accounts.push(address);
            diff.new_accounts.push(NewAccount {
                address,
                balance: U256::from(n),
                nonce: n,
                code_hash: H256::ZERO,
            });
            let slot = (key(2, n), key(3, n));
            storage_keys.push(slot);
            diff.storage_diffs.push(AccountStorageDiff {
                address: slot.0,
                diffs: vec![IndexValuePair {
                    index: slot.1,
                    value: U256::from(n + 1),
                }],
            });
        }
        let number = chunk_start / BLOCK_CHUNK + 1;
        let hash = key(4, number);
        state
            .update_block(block_info(number, hash, parent), diff)
            .unwrap();
        parent = hash;
    }
    db.flush_all();
    Fixture {
        db,
        dir,
        accounts,
        storage_keys,
    }
}

/// Deterministic rotating sample: stride through the keyspace so each
/// iteration touches a different window.
fn sample_indices(cursor: &mut u64, count: usize) -> Vec<usize> {
    (0..count)
        .map(|_| {
            *cursor = cursor
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((*cursor >> 16) % KEYSPACE) as usize
        })
        .collect()
}

fn bench_batch_read(c: &mut Criterion) {
    let fixture = setup();
    let db = &fixture.db;

    let mut group = c.benchmark_group("rocksdb_storage_read");
    for &count in &[8usize, 32] {
        group.throughput(Throughput::Elements(count as u64));
        let mut cursor = 1u64;
        group.bench_with_input(
            BenchmarkId::new("scalar_loop", count),
            &count,
            |b, &count| {
                b.iter(|| {
                    for i in sample_indices(&mut cursor, count) {
                        let (address, slot) = fixture.storage_keys[i];
                        black_box(db.read_storage(address, slot).unwrap());
                    }
                })
            },
        );
        let mut cursor = 1u64;
        group.bench_with_input(BenchmarkId::new("multi_get", count), &count, |b, &count| {
            b.iter(|| {
                let keys: Vec<(H256, H256)> = sample_indices(&mut cursor, count)
                    .into_iter()
                    .map(|i| fixture.storage_keys[i])
                    .collect();
                black_box(db.read_storage_many(&keys).unwrap());
            })
        });
    }
    group.finish();

    let mut group = c.benchmark_group("rocksdb_account_read");
    for &count in &[8usize, 32] {
        group.throughput(Throughput::Elements(count as u64));
        let mut cursor = 2u64;
        group.bench_with_input(
            BenchmarkId::new("scalar_loop", count),
            &count,
            |b, &count| {
                b.iter(|| {
                    for i in sample_indices(&mut cursor, count) {
                        black_box(db.read_account(fixture.accounts[i]).unwrap());
                    }
                })
            },
        );
        let mut cursor = 2u64;
        group.bench_with_input(BenchmarkId::new("multi_get", count), &count, |b, &count| {
            b.iter(|| {
                let keys: Vec<H256> = sample_indices(&mut cursor, count)
                    .into_iter()
                    .map(|i| fixture.accounts[i])
                    .collect();
                black_box(db.read_account_many(&keys).unwrap());
            })
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(30);
    targets = bench_batch_read
}
criterion_main!(benches);
