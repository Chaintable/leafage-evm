//! Warm-SST archive cache benchmark; setup and cache resets are not timed.
//! Run the ignored release test with `--nocapture --test-threads=1`.
use super::*;
use std::hint::black_box;
use std::time::Instant;

const HEAD: u64 = 20_000;
const KEYS: usize = 512;
const SLOT: H256 = H256::repeat_byte(0x5a);

fn state(db: &Arc<DataBaseRef>, block_num: u64) -> StateDB {
    let (iterators, cache_view) = new_state_iterators(db, block_num);
    StateDB {
        db: db.clone(),
        block_num,
        block_info: None,
        iterators,
        cache_view,
        tracker_id: None,
        timed_out: None,
        udt: Some(udt::UdtReader::new(block_num)),
        udt_head: std::sync::OnceLock::new(),
    }
}

fn read(state: &StateDB, key: H256, accounts: bool) -> u64 {
    if accounts {
        state.read_account(key).unwrap().unwrap().nonce
    } else {
        state.read_storage(key, SLOT).unwrap().to::<u64>()
    }
}

#[test]
#[ignore = "release benchmark; see docs/performance/archive-state-intervals.md"]
fn bench_archive_state_intervals() {
    let _lock = ARCHIVE_DB_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("leafage-interval-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    udt::init_archive_schema(&dir, udt::ArchiveSchema::V3).unwrap();
    let mut db = Arc::new(DataBaseRef::open(&dir, 64, true, false));
    let keys: Vec<Vec<_>> = (0..2u64)
        .map(|group| {
            (0..KEYS)
                .map(|i| alloy::primitives::keccak256(((group << 32) | i as u64).to_le_bytes()))
                .collect()
        })
        .collect();
    let account = db
        .db
        .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
        .unwrap();
    let storage = db
        .db
        .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
        .unwrap();
    for (group, keys) in keys.iter().enumerate() {
        let versions: Vec<_> = if group == 0 {
            vec![100, 10_000]
        } else {
            (100..164).chain([10_000]).collect()
        };
        let mut batch = WriteBatch::default();
        for version in versions {
            for &address in keys {
                let mut key = [0; 64];
                key[..32].copy_from_slice(address.as_slice());
                key[32..].copy_from_slice(SLOT.as_slice());
                batch.put_cf_with_ts(storage, key, udt::encode_ts(version), version.to_be_bytes());
                batch.put_cf_with_ts(
                    account,
                    address,
                    udt::encode_ts(version),
                    encode_slim_account(NewAccount {
                        address,
                        balance: U256::from(version),
                        nonce: version,
                        code_hash: KECCAK256_EMPTY,
                    }),
                );
            }
        }
        db.db.write(batch).unwrap();
        db.flush().unwrap();
    }
    db.compact().unwrap();
    // Warm both CFs and both ends of their history before any measurements.
    for height in [101, HEAD] {
        for &key in keys.iter().flatten() {
            for accounts in [false, true] {
                black_box(read(&state(&db, height), key, accounts));
            }
        }
    }
    for accounts in [false, true] {
        for (case, group, sweeps) in [
            ("sparse_2", 0, 2),
            ("sparse_4", 0, 4),
            ("sparse_8", 0, 8),
            ("sparse_16", 0, 16),
            ("sparse_1024", 0, 1024),
            ("head", 0, 64),
            ("dense", 1, 64),
        ] {
            let cycles = if sweeps <= 16 { 64 } else { 1 };
            let mut elapsed = 0;
            for _ in 0..cycles {
                let cache = StateCache::new(16 << 20);
                cache.advance_head(HEAD);
                Arc::get_mut(&mut db).unwrap().state_cache = Some(cache);
                // Common workload: the newest version is already cached.
                let head = state(&db, HEAD);
                for &key in &keys[group] {
                    assert_eq!(read(&head, key, accounts), 10_000);
                }
                drop(head);
                let states: Vec<_> = (0..sweeps)
                    .map(|i| state(&db, if case == "head" { HEAD } else { 100 + i }))
                    .collect();
                let start = Instant::now();
                for s in &states {
                    let expected = if case == "head" {
                        10_000
                    } else if group == 1 {
                        s.block_num
                    } else {
                        100
                    };
                    for &key in &keys[group] {
                        assert_eq!(black_box(read(s, key, accounts)), expected);
                    }
                }
                elapsed += start.elapsed().as_nanos();
            }
            println!(
                "INTERVAL_BENCH,{},{case},{},{elapsed}",
                if accounts { "account" } else { "storage" },
                cycles * KEYS * sweeps as usize
            );
        }
    }
    drop(db);
    std::fs::remove_dir_all(dir).unwrap();
}
