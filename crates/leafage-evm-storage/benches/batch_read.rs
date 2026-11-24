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
//!
//! `archive_v3` compares the full archive scalar/batch paths at block 2
//! with three versions flushed and compacted to SST. Samples are prepared
//! outside timing; both paths return the same Vec and are checked first.
//! Run cache configurations in separate processes (no process-global env
//! mutations while RocksDB background threads are running):
//! ARCHIVE_STATE_CACHE_MB=0 ARCHIVE_CODE_CACHE_MB=0 cargo bench -p leafage-evm-storage --bench batch_read -- archive_v3
//! ARCHIVE_STATE_CACHE_MB=64 ARCHIVE_CODE_CACHE_MB=64 cargo bench -p leafage-evm-storage --bench batch_read -- archive_v3
//! BENCH_BLOCK_CACHE_MB=1 ARCHIVE_STATE_CACHE_MB=0 ARCHIVE_CODE_CACHE_MB=0 cargo bench -p leafage-evm-storage --bench batch_read -- archive_v3
//! The small block-cache case still has a warm OS page cache, not cold disk.

use alloy::primitives::keccak256;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use leafage_evm_storage::{
    init_archive_schema, set_inverted_block_encoding, ArchiveRocksDBStorage, ArchiveSchema,
    ArchiveStateDB, EvmStorageWrite, RocksDBStorage, StateDBProvider, StateDBRead, StateDBWrapper,
};
use leafage_evm_types::{
    AccountStorageDiff, Block, BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff, Bytes,
    IndexValuePair, NewAccount, NewCode, H256, U256,
};
use std::sync::Arc;
use std::time::Duration;

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

struct Fixture<DB> {
    db: DB,
    dir: std::path::PathBuf,
    accounts: Vec<H256>,
    storage_keys: Vec<(H256, H256)>,
    code_hashes: Vec<H256>,
}

impl<DB> Drop for Fixture<DB> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup() -> Fixture<Arc<RocksDBStorage>> {
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
        code_hashes: Vec::new(),
    }
}

/// Deterministic rotating sample: stride through the keyspace so each
/// iteration touches a different window.
fn sample_indices(cursor: &mut u64, count: usize, keyspace: usize) -> Vec<usize> {
    (0..count)
        .map(|_| {
            *cursor = cursor
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((*cursor >> 16) % keyspace as u64) as usize
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
                    for i in sample_indices(&mut cursor, count, KEYSPACE as usize) {
                        let (address, slot) = fixture.storage_keys[i];
                        black_box(db.read_storage(address, slot).unwrap());
                    }
                })
            },
        );
        let mut cursor = 1u64;
        group.bench_with_input(BenchmarkId::new("multi_get", count), &count, |b, &count| {
            b.iter(|| {
                let keys: Vec<(H256, H256)> = sample_indices(&mut cursor, count, KEYSPACE as usize)
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
                    for i in sample_indices(&mut cursor, count, KEYSPACE as usize) {
                        black_box(db.read_account(fixture.accounts[i]).unwrap());
                    }
                })
            },
        );
        let mut cursor = 2u64;
        group.bench_with_input(BenchmarkId::new("multi_get", count), &count, |b, &count| {
            b.iter(|| {
                let keys: Vec<H256> = sample_indices(&mut cursor, count, KEYSPACE as usize)
                    .into_iter()
                    .map(|i| fixture.accounts[i])
                    .collect();
                black_box(db.read_account_many(&keys).unwrap());
            })
        });
    }
    group.finish();
}

fn setup_archive(block_cache_mb: usize) -> Fixture<ArchiveStateDB> {
    const KEYS: usize = 65_536;
    let dir = std::env::temp_dir().join(format!(
        "leafage-bench-archive-batch-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    init_archive_schema(&dir, ArchiveSchema::V3).unwrap();
    set_inverted_block_encoding(true);
    let db = Arc::new(ArchiveRocksDBStorage::open(
        &dir,
        block_cache_mb,
        true,
        false,
    ));
    let accounts: Vec<_> = (0..KEYS).map(|i| keccak256(key(1, i as u64))).collect();
    let storage_keys: Vec<_> = accounts
        .iter()
        .enumerate()
        .map(|(i, address)| (*address, keccak256(key(2, i as u64))))
        .collect();
    let codes: Vec<_> = (0..KEYS)
        .map(|i| {
            let mut seed = i as u64 + 1;
            let code = Bytes::from(
                (0..512)
                    .map(|_| {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        (seed >> 32) as u8
                    })
                    .collect::<Vec<_>>(),
            );
            NewCode {
                code_hash: keccak256(&code),
                code,
            }
        })
        .collect();
    let code_hashes = codes.iter().map(|code| code.code_hash).collect();
    let mut parent = H256::ZERO;
    for height in 1..=3u64 {
        let mut diff = BlockStorageDiff::default();
        for (i, address) in accounts.iter().enumerate() {
            diff.new_accounts.push(NewAccount {
                address: *address,
                balance: U256::from(i as u64 + height),
                nonce: height,
                code_hash: codes[i].code_hash,
            });
            diff.storage_diffs.push(AccountStorageDiff {
                address: *address,
                diffs: vec![IndexValuePair {
                    index: storage_keys[i].1,
                    value: U256::from(i as u64 + height),
                }],
            });
        }
        if height == 1 {
            diff.new_codes = codes.clone();
        }
        let hash = key(4, height);
        StateDBWrapper(
            db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
                .unwrap()
                .unwrap(),
        )
        .update_block(block_info(height, hash, parent), diff)
        .unwrap();
        db.flush().unwrap();
        parent = hash;
    }
    db.compact().unwrap();
    let state = db
        .db_at(BlockId::Number(BlockNumberOrTag::Number(2)))
        .unwrap()
        .unwrap();
    // Check the requested historical version independently of scalar/batch parity.
    assert_eq!(state.read_account(accounts[0]).unwrap().unwrap().nonce, 2);
    assert_eq!(
        state
            .read_storage(storage_keys[0].0, storage_keys[0].1)
            .unwrap(),
        U256::from(2)
    );
    Fixture {
        db: state,
        dir,
        accounts,
        storage_keys,
        code_hashes,
    }
}

fn compare_archive_reads<K: Copy, V: std::fmt::Debug + PartialEq>(
    c: &mut Criterion,
    name: &str,
    keys: &[K],
    scalar: impl Fn(&[K]) -> Vec<V>,
    batch: impl Fn(&[K]) -> Vec<V>,
) {
    let mut group = c.benchmark_group(name);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));
    group.sample_size(20);
    for count in [1, 8, 32, 64] {
        let mut cursor = 1;
        let samples: Vec<Vec<K>> = (0..1024)
            .map(|_| {
                sample_indices(&mut cursor, count, keys.len())
                    .into_iter()
                    .map(|i| keys[i])
                    .collect()
            })
            .collect();
        // Warm both paths equally and validate every timed input. With caches
        // enabled these fit the cache; with caches disabled this warms SST pages.
        for sample in &samples {
            assert_eq!(scalar(sample), batch(sample));
        }
        group.throughput(Throughput::Elements(count as u64));
        for (method, read) in [
            ("scalar_loop", &scalar as &dyn Fn(&[K]) -> Vec<V>),
            ("batch", &batch as &dyn Fn(&[K]) -> Vec<V>),
        ] {
            let mut cursor = 0;
            group.bench_function(BenchmarkId::new(method, count), |b| {
                b.iter(|| {
                    let input = &samples[cursor];
                    cursor = (cursor + 1) % samples.len();
                    black_box(read(black_box(input)))
                })
            });
        }
    }
    group.finish();
}

fn bench_archive_read(c: &mut Criterion) {
    let block_cache_mb = std::env::var("BENCH_BLOCK_CACHE_MB")
        .map(|s| {
            s.parse::<usize>()
                .expect("BENCH_BLOCK_CACHE_MB must be an integer")
        })
        .unwrap_or(256);
    let state_cache = std::env::var("ARCHIVE_STATE_CACHE_MB").unwrap_or("default".into());
    let code_cache = std::env::var("ARCHIVE_CODE_CACHE_MB").unwrap_or("default".into());
    let name = format!("archive_v3/state{state_cache}_code{code_cache}_block{block_cache_mb}");
    let fixture = setup_archive(block_cache_mb);
    let db = &fixture.db;
    compare_archive_reads(
        c,
        &format!("{name}/storage"),
        &fixture.storage_keys,
        |keys| {
            keys.iter()
                .map(|(a, s)| db.read_storage(*a, *s).unwrap())
                .collect()
        },
        |keys| db.read_storage_many(keys).unwrap(),
    );
    compare_archive_reads(
        c,
        &format!("{name}/account"),
        &fixture.accounts,
        |keys| keys.iter().map(|a| db.read_account(*a).unwrap()).collect(),
        |keys| db.read_account_many(keys).unwrap(),
    );
    compare_archive_reads(
        c,
        &format!("{name}/code"),
        &fixture.code_hashes,
        |keys| {
            keys.iter()
                .map(|hash| db.read_code(*hash).unwrap())
                .collect()
        },
        |keys| db.read_code_many(keys).unwrap(),
    );
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(30);
    targets = bench_batch_read, bench_archive_read
}
criterion_main!(benches);
