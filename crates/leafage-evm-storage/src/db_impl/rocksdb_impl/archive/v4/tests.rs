//! Schema v4 through the archive: it must answer every read as schema v3
//! does, across restarts, flushes, base rewrites and rewinds.

use super::super::udt::{init_archive_schema, ArchiveSchema};
use super::super::*;
use crate::db::{StateDBRead, StateDBWrapper};
use crate::db_impl::{set_inverted_block_encoding, MultiStorage, StorageKind};
use crate::interface::EvmStorageWrite;
use leafage_evm_types::{AccountStorageDiff, BlockStorageDiff, IndexValuePair};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Sets environment variables for one test and restores them after.
struct Env(Vec<(&'static str, Option<String>)>);

impl Env {
    fn set(vars: &[(&'static str, &str)]) -> Self {
        Self(
            vars.iter()
                .map(|(name, value)| {
                    let old = std::env::var(name).ok();
                    std::env::set_var(name, value);
                    (*name, old)
                })
                .collect(),
        )
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (name, old) in self.0.drain(..) {
            match old {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn block_info(number: u64, fork: u8) -> BlockInfo {
    let mut hash = H256::ZERO;
    hash.0[24..].copy_from_slice(&number.to_be_bytes());
    hash.0[0] = 0xb1;
    hash.0[1] = fork;
    BlockInfo::new(Block {
        header: Header {
            hash,
            inner: RawHeader {
                number,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    })
}

fn addrs() -> Vec<H256> {
    (0..14u8)
        .map(|i| {
            let mut a = H256::repeat_byte(i.wrapping_mul(37) | 1);
            a.0[0] = [0x05, 0x45, 0x85, 0xc5, 0x05, 0x46, 0xf0][i as usize % 7];
            // Two pairs equal in their first ten bytes.
            if i >= 10 {
                a.0[..10].fill(0x33 + (i % 2));
            }
            a.0[31] = i;
            a
        })
        .collect()
}

fn slot(a: usize, s: u64) -> H256 {
    let mut r = Rng((a as u64 * 1_000_003 + s).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut h = H256::ZERO;
    for chunk in h.0.chunks_mut(8) {
        chunk.copy_from_slice(&r.below(u64::MAX).to_be_bytes());
    }
    h
}

/// Address 0 is a large contract; the others have a few slots.
fn slots_of(a: usize) -> u64 {
    if a == 0 {
        900
    } else {
        5
    }
}

fn random_diff(rng: &mut Rng, n: u64) -> BlockStorageDiff {
    let mut diff = BlockStorageDiff::default();
    for (i, a) in addrs().into_iter().enumerate() {
        match rng.below(12) {
            0 if n > 3 && i != 0 => diff.deleted_accounts.push(a),
            1..=5 => {
                diff.new_accounts.push(NewAccount {
                    address: a,
                    balance: U256::from(rng.below(1_000_000)) << (rng.below(3) * 60) as usize,
                    nonce: n / (1 + i as u64),
                    code_hash: if i % 3 == 0 {
                        KECCAK256_EMPTY.0.into()
                    } else {
                        H256::repeat_byte(0xc0 + i as u8)
                    },
                });
                let mut diffs = Vec::new();
                for _ in 0..rng.below(if i == 0 { 60 } else { 4 }) {
                    let value = match rng.below(4) {
                        0 => U256::ZERO,
                        1 => U256::from(rng.below(256)),
                        2 => U256::from(rng.below(u64::MAX)),
                        _ => U256::MAX - U256::from(rng.below(1000)),
                    };
                    diffs.push(IndexValuePair {
                        index: slot(i, rng.below(slots_of(i))),
                        value,
                    });
                }
                if !diffs.is_empty() {
                    diff.storage_diffs
                        .push(AccountStorageDiff { address: a, diffs });
                }
            }
            _ => {}
        }
    }
    diff
}

fn open(dir: &Path) -> Arc<DataBaseRef> {
    set_inverted_block_encoding(true);
    Arc::new(DataBaseRef::open(dir, 64, false, false))
}

fn latest(db: &Arc<DataBaseRef>) -> StateDBWrapper<StateDB> {
    StateDBWrapper(
        db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap(),
    )
}

fn at(db: &Arc<DataBaseRef>, n: u64) -> StateDB {
    db.db_at(BlockId::Number(BlockNumberOrTag::Number(n)))
        .unwrap()
        .unwrap()
}

/// What a set of reads returns: accounts, slots with the block that wrote
/// them, and the non-zero slots of some addresses, at a spread of heights.
#[derive(Debug, PartialEq, Default)]
struct Answers {
    accounts: BTreeMap<(usize, u64), Option<NewAccount>>,
    slots: BTreeMap<(usize, u64, u64), (U256, Option<u64>)>,
    listed: BTreeMap<(usize, u64), Vec<(H256, U256)>>,
}

fn answers(db: &Arc<DataBaseRef>, head: u64) -> Answers {
    let mut out = Answers::default();
    let heights: Vec<u64> = (1..=head).filter(|h| h % 7 == 1 || *h + 3 > head).collect();
    for &h in &heights {
        let state = at(db, h);
        for (i, a) in addrs().into_iter().enumerate() {
            out.accounts.insert((i, h), state.read_account(a).unwrap());
            // Slots written and slots never written.
            for s in (0..slots_of(i) + 2).step_by(if i == 0 { 13 } else { 1 }) {
                out.slots.insert(
                    (i, s, h),
                    state.read_storage_and_height(a, slot(i, s)).unwrap(),
                );
            }
            if h % 21 == 1 || h == head {
                out.listed
                    .insert((i, h), state.read_storage_slots(a).unwrap().unwrap());
            }
        }
        // Batched reads agree with single ones.
        let keys: Vec<(H256, H256)> = (0..7).map(|s| (addrs()[1], slot(1, s))).collect();
        let many = state.read_storage_many(&keys).unwrap();
        for (s, value) in many.into_iter().enumerate() {
            assert_eq!(value, out.slots[&(1, s as u64, h)].0);
        }
        let many = state.read_account_many(&addrs()).unwrap();
        for (i, account) in many.into_iter().enumerate() {
            assert_eq!(account, out.accounts[&(i, h)]);
        }
    }
    out
}

fn test_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("leafage-v4-archive-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn history(seed: u64, blocks: u64) -> Vec<BlockStorageDiff> {
    let mut rng = Rng(seed);
    (1..=blocks).map(|n| random_diff(&mut rng, n)).collect()
}

fn head(db: &Arc<DataBaseRef>) -> u64 {
    latest(db)
        .last_committed_block()
        .unwrap()
        .unwrap()
        .header
        .number
}

/// Write the memtable to a segment, as the background does when it is due.
fn flush(db: &Arc<DataBaseRef>) {
    db.v4.as_ref().unwrap().flush().unwrap();
}

fn write(db: &Arc<DataBaseRef>, from: u64, diffs: &[BlockStorageDiff], fork: u8) {
    for (i, diff) in diffs.iter().enumerate() {
        latest(db)
            .update_block(block_info(from + i as u64, fork), diff.clone())
            .unwrap();
    }
}

/// Small segments and base files, a rewrite as soon as there is a delta.
fn small_store() -> Env {
    Env::set(&[
        ("ARCHIVE_V4_FLUSH_BLOCKS", "9"),
        ("ARCHIVE_V4_TIERS", "3"),
        ("ARCHIVE_V4_BASE_FILE_MB", "0.02"),
        ("ARCHIVE_V4_DELTA_MIN_MB", "0"),
        ("ARCHIVE_V4_DELTA_PERCENT", "30"),
        ("ARCHIVE_V4_REWRITE_MBPS", "0"),
        ("ARCHIVE_V4_REWRITE_CHECK_SECS", "0.02"),
    ])
}

#[test]
fn v4_answers_as_v3_across_restarts_and_rewrites() {
    let _g = ARCHIVE_DB_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = small_store();
    let diffs = history(0x5eed_0001, 120);
    let (v3_dir, v4_dir) = (test_dir("ref"), test_dir("v4"));

    init_archive_schema(&v3_dir, ArchiveSchema::V3).unwrap();
    let db = open(&v3_dir);
    assert!(db.is_schema_v3());
    write(&db, 1, &diffs, 0);
    let want_80 = {
        // The answers up to block 80 do not change with later blocks.
        let mut a = answers(&db, 120);
        a.accounts.retain(|k, _| k.1 <= 80);
        a.slots.retain(|k, _| k.2 <= 80);
        a.listed.retain(|k, _| k.1 <= 80);
        a
    };
    let want = answers(&db, 120);
    drop(db);

    assert!(init_archive_schema(&v4_dir, ArchiveSchema::V4).unwrap());
    let db = open(&v4_dir);
    assert!(db.is_schema_v4() && !db.takes_version_ssts());
    db.write_encoding_marker(true).unwrap();
    write(&db, 1, &diffs[..80], 0);
    // Twice: the second pass is served by the state cache where it can be.
    for _ in 0..2 {
        let mut got = answers(&db, 80);
        got.accounts.retain(|k, _| want_80.accounts.contains_key(k));
        got.slots.retain(|k, _| want_80.slots.contains_key(k));
        got.listed.retain(|k, _| want_80.listed.contains_key(k));
        // (Heights near the head differ between the two sets of reads.)
        for (k, v) in &got.slots {
            assert_eq!(Some(v), want_80.slots.get(k), "slot {k:?}");
        }
        for (k, v) in &got.accounts {
            assert_eq!(Some(v), want_80.accounts.get(k), "account {k:?}");
        }
        for (k, v) in &got.listed {
            assert_eq!(Some(v), want_80.listed.get(k), "slots of {k:?}");
        }
    }
    let (base_files, _, segments, ..) = db.v4_sizes().unwrap();
    assert!(base_files + segments > 0, "something was flushed");
    drop(db);

    // Restart: the blocks still in the memtable are gone, and the head is
    // the last flushed block. They are written again, as the node does.
    let db = open(&v4_dir);
    let reopened = head(&db);
    assert!((1..=80).contains(&reopened), "head {reopened}");
    write(&db, reopened + 1, &diffs[reopened as usize..], 0);
    assert_eq!(answers(&db, 120), want);
    flush(&db);
    drop(db);

    let db = open(&v4_dir);
    assert_eq!(head(&db), 120);
    assert_eq!(answers(&db, 120), want);
    // Everything into the base, then again from the files alone.
    db.v4_compact().unwrap();
    assert_eq!(answers(&db, 120), want);
    let (base_files, _, segments, ..) = db.v4_sizes().unwrap();
    assert!(
        base_files > 2 && segments == 0,
        "{base_files} base files, {segments} segments"
    );
    drop(db);
    let db = open(&v4_dir);
    assert_eq!(answers(&db, 120), want);

    // Writing below the head is refused, with or without new block infos.
    let mut batch = db.prepare_write_batch().unwrap();
    db.write_storage(&mut batch, addrs()[1], slot(1, 0), 50, U256::from(1))
        .unwrap();
    assert!(db.commit(batch).is_err());
    drop(db);
    let _ = std::fs::remove_dir_all(&v3_dir);
    let _ = std::fs::remove_dir_all(&v4_dir);
}

#[test]
fn v4_rewind_goes_back_to_a_segment_end_and_takes_another_chain() {
    let _g = ARCHIVE_DB_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Segments of 9 blocks, merged by 8; no base until the test makes one.
    let _env = Env::set(&[
        ("ARCHIVE_V4_FLUSH_BLOCKS", "9"),
        ("ARCHIVE_V4_TIERS", "8"),
        ("ARCHIVE_V4_BASE_FILE_MB", "0.02"),
        ("ARCHIVE_V4_DELTA_MIN_MB", "1000000"),
    ]);
    let first = history(0x5eed_0002, 90);
    let second = history(0x5eed_0003, 90);
    let (v3_dir, v4_dir) = (test_dir("rewind-ref"), test_dir("rewind-v4"));

    init_archive_schema(&v4_dir, ArchiveSchema::V4).unwrap();
    let db = open(&v4_dir);
    db.write_encoding_marker(true).unwrap();
    write(&db, 1, &first, 0);
    flush(&db);
    drop(db);
    // Twice: each time the archive ends at the last segment end at or
    // below the target, and says so.
    let mut landed = 90;
    for target in [85, 77] {
        let rewound = MultiStorage::rewind_archive_offline(
            &v4_dir,
            64,
            StorageKind::Rocksdb,
            target,
            None,
            &v4_dir.join("no-offset"),
        )
        .unwrap();
        let number = rewound.header.number;
        assert!(
            number <= target && number < landed && number % 9 == 0,
            "block {number}"
        );
        assert_eq!(rewound.header.hash, block_info(number, 0).header.hash);
        landed = number;
        let db = open(&v4_dir);
        assert_eq!(head(&db), landed);
        drop(db);
    }
    // The reference: the first chain to that block, then the second.
    init_archive_schema(&v3_dir, ArchiveSchema::V3).unwrap();
    let db = open(&v3_dir);
    write(&db, 1, &first[..landed as usize], 0);
    write(&db, landed + 1, &second[landed as usize..], 1);
    let want = answers(&db, 90);
    drop(db);

    let db = open(&v4_dir);
    write(&db, landed + 1, &second[landed as usize..], 1);
    assert_eq!(answers(&db, 90), want);
    // Everything into the base: there is no way back into it.
    db.v4_compact().unwrap();
    drop(db);
    let db = open(&v4_dir);
    assert_eq!(head(&db), 90);
    assert_eq!(answers(&db, 90), want);
    drop(db);
    assert!(MultiStorage::rewind_archive_offline(
        &v4_dir,
        64,
        StorageKind::Rocksdb,
        50,
        None,
        &v4_dir.join("no-offset"),
    )
    .is_err());
    let db = open(&v4_dir);
    assert_eq!(head(&db), 90);
    assert_eq!(answers(&db, 90), want);
    drop(db);
    let _ = std::fs::remove_dir_all(&v3_dir);
    let _ = std::fs::remove_dir_all(&v4_dir);
}

#[test]
fn v4_reads_give_the_state_cache_whole_spans() {
    let _g = ARCHIVE_DB_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = small_store();
    let dir = test_dir("spans");
    init_archive_schema(&dir, ArchiveSchema::V4).unwrap();
    let db = open(&dir);
    db.write_encoding_marker(true).unwrap();
    let (a, s) = (addrs()[2], slot(2, 1));
    let key = {
        let mut k = [0u8; 64];
        k[..32].copy_from_slice(a.as_slice());
        k[32..].copy_from_slice(s.as_slice());
        k
    };
    let set = |value: u64| BlockStorageDiff {
        storage_diffs: vec![AccountStorageDiff {
            address: a,
            diffs: vec![IndexValuePair {
                index: s,
                value: U256::from(value),
            }],
        }],
        ..Default::default()
    };
    for n in 1..=60u64 {
        let diff = match n {
            10 => set(1),
            // The first block of a memtable of 9: a segment starts there.
            37 => set(2),
            _ => BlockStorageDiff::default(),
        };
        latest(&db).update_block(block_info(n, 0), diff).unwrap();
    }
    let cache = db
        .state_cache
        .as_ref()
        .expect("the state cache is on by default");
    let cached = |block: u64| match cache.storage.lookup(&key, block) {
        Lookup::Hit(span) => Some((span.lo, span.hi, span.found, span.value)),
        Lookup::Miss(_) => None,
    };
    assert_eq!(cached(20), None);
    // One read between the versions learns the whole span, with no probe.
    assert_eq!(
        at(&db, 20).read_storage_and_height(a, s).unwrap(),
        (U256::from(1), Some(10))
    );
    assert_eq!(cached(10), Some((10, 36, true, U256::from(1))));
    assert_eq!(cached(36), Some((10, 36, true, U256::from(1))));
    assert_eq!(cached(37), None);
    assert_eq!(cached(9), None);
    // Before the first version: absent up to the block before it.
    assert_eq!(
        at(&db, 3).read_storage_and_height(a, s).unwrap(),
        (U256::ZERO, None)
    );
    assert_eq!(cached(9), Some((0, 9, false, U256::ZERO)));
    // The newest version holds up to the head the reader saw.
    assert_eq!(
        at(&db, 45).read_storage_and_height(a, s).unwrap(),
        (U256::from(2), Some(37))
    );
    assert_eq!(cached(60), Some((37, 60, true, U256::from(2))));
    // A later block ends it there, and the cache does not answer above.
    latest(&db).update_block(block_info(61, 0), set(3)).unwrap();
    assert_eq!(cached(61), None);
    assert_eq!(
        at(&db, 61).read_storage_and_height(a, s).unwrap(),
        (U256::from(3), Some(61))
    );
    assert_eq!(cached(60), Some((37, 60, true, U256::from(2))));
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else if entry.file_name() != "LOCK" {
            // A file may go while the directory is read (RocksDB's own).
            let _ = std::fs::copy(entry.path(), target);
        }
    }
}

/// Images of the directory taken while the node writes and the store
/// flushes, merges and rewrites in the background are what crashes at those
/// moments leave: partly written files, files no longer listed, block infos
/// of blocks that were only in the memtable. Each must open at a block it
/// holds whole, and be at the state of the image's block once the blocks
/// after that one are written again.
#[test]
fn v4_recovers_from_a_crash_at_any_moment() {
    let _g = ARCHIVE_DB_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = small_store();
    let diffs = history(0x5eed_0004, 150);
    let (v3_dir, v4_dir) = (test_dir("crash-ref"), test_dir("crash-v4"));
    let images = test_dir("crash-images");
    let points = [7u64, 23, 40, 58, 77, 96, 121, 150];

    init_archive_schema(&v3_dir, ArchiveSchema::V3).unwrap();
    let db = open(&v3_dir);
    let mut want = Vec::new();
    for (i, diff) in diffs.iter().enumerate() {
        let n = i as u64 + 1;
        latest(&db)
            .update_block(block_info(n, 0), diff.clone())
            .unwrap();
        if points.contains(&n) {
            want.push(answers(&db, n));
        }
    }
    drop(db);

    init_archive_schema(&v4_dir, ArchiveSchema::V4).unwrap();
    let db = open(&v4_dir);
    db.write_encoding_marker(true).unwrap();
    for (i, diff) in diffs.iter().enumerate() {
        let n = i as u64 + 1;
        latest(&db)
            .update_block(block_info(n, 0), diff.clone())
            .unwrap();
        if points.contains(&n) {
            // Let the background get into something.
            std::thread::sleep(std::time::Duration::from_millis(n % 7 * 3));
            let _held = db.v4_crash_point();
            copy_dir(&v4_dir, &images.join(n.to_string()));
        }
    }
    drop(db);

    let (mut leftovers, mut reopened_below) = (0, 0);
    for (n, want) in points.iter().zip(&want) {
        let dir = images.join(n.to_string());
        let files = |dir: &Path| std::fs::read_dir(dir.join("v4")).unwrap().count();
        let before = files(&dir);
        let db = open(&dir);
        // Nothing may have been flushed yet: then the archive is empty.
        let reopened = db.read_latest_block_num().unwrap().unwrap_or(0);
        assert!(reopened <= *n, "head {reopened} of the image at block {n}");
        reopened_below += u64::from(reopened < *n);
        leftovers += before - files(&dir).min(before);
        write(&db, reopened + 1, &diffs[reopened as usize..*n as usize], 0);
        assert_eq!(&answers(&db, *n), want, "image at block {n}");
        // And it goes on from there.
        if let Some(next) = diffs.get(*n as usize) {
            latest(&db)
                .update_block(block_info(n + 1, 0), next.clone())
                .unwrap();
        }
        drop(db);
    }
    // (Not asserted: whether an image caught a file half written depends
    // on timing.)
    let _ = leftovers;
    // Points that are no multiple of the flush interval lost blocks.
    assert!(reopened_below >= 4, "{reopened_below} images lost blocks");
    for dir in [v3_dir, v4_dir, images] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Blocks that were never flushed are gone after a restart, with their
/// infos: the head is the last flushed block, the archive
/// answers as it did at that block, and takes the blocks again, of another
/// chain too.
#[test]
fn v4_restart_goes_back_to_the_last_flushed_block() {
    let _g = ARCHIVE_DB_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Nothing is flushed unless the test asks for it.
    let _env = Env::set(&[
        ("ARCHIVE_V4_FLUSH_BLOCKS", "1000000"),
        ("ARCHIVE_V4_DELTA_MIN_MB", "1000000"),
    ]);
    let first = history(0x5eed_0005, 40);
    let second = history(0x5eed_0006, 40);
    let (v3_dir, v4_dir) = (test_dir("tail-ref"), test_dir("tail-v4"));

    // The reference: the first chain to block 25, then the second.
    init_archive_schema(&v3_dir, ArchiveSchema::V3).unwrap();
    let db = open(&v3_dir);
    write(&db, 1, &first[..25], 0);
    let want_25 = answers(&db, 25);
    write(&db, 26, &second[25..], 1);
    let want = answers(&db, 40);
    drop(db);

    init_archive_schema(&v4_dir, ArchiveSchema::V4).unwrap();
    let db = open(&v4_dir);
    db.write_encoding_marker(true).unwrap();
    // Nothing flushed: the archive opens empty and starts over.
    write(&db, 1, &first[..7], 0);
    drop(db);
    let db = open(&v4_dir);
    assert_eq!(db.read_latest_block_num().unwrap(), None);
    write(&db, 1, &first[..25], 0);
    flush(&db);
    write(&db, 26, &first[25..], 0);
    assert_eq!(head(&db), 40);
    drop(db);

    for _ in 0..2 {
        let db = open(&v4_dir);
        assert_eq!(head(&db), 25);
        assert_eq!(answers(&db, 25), want_25);
        drop(db);
    }
    let db = open(&v4_dir);
    write(&db, 26, &second[25..], 1);
    assert_eq!(answers(&db, 40), want);
    flush(&db);
    drop(db);
    let db = open(&v4_dir);
    assert_eq!(head(&db), 40);
    assert_eq!(answers(&db, 40), want);
    drop(db);
    let _ = std::fs::remove_dir_all(&v3_dir);
    let _ = std::fs::remove_dir_all(&v4_dir);
}
