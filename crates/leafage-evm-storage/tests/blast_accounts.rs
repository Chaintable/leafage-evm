//! Blast raw accounts through every storage backend and the in-memory layers.
//!
//! The account codec is process-wide, so these tests live in their own binary
//! and only ever run with it set to `BlastV1`.

use leafage_evm_storage::{
    set_inverted_block_encoding, set_state_diff_codec, EvmStorageRead, EvmStorageWrite,
    LatestStateDBIterator, MultiStorage, StateDB, StateDBProvider, StateDBRead, StateDBWrapper,
    StateTree, StateTreeConfig, StorageKind,
};
use leafage_evm_types::{
    AccountExt, BlastAccountExt, BlastBlockStorageDiff, BlastNewAccount, Block, BlockId, BlockInfo,
    BlockStateUpdate, BlockStorageDiff, Header, NewAccount, RawHeader, StateDiffCodec,
    StoredAccount, H256, KECCAK256_EMPTY, U256,
};
use std::path::PathBuf;
use std::sync::Mutex;

/// The RocksDB archive backend keeps its DB in a process-global static, so
/// tests open one DB at a time.
static DB_LOCK: Mutex<()> = Mutex::new(());

const BACKENDS: [(StorageKind, bool); 4] = [
    (StorageKind::Rocksdb, false),
    (StorageKind::Rocksdb, true),
    (StorageKind::MDBX, false),
    (StorageKind::MDBX, true),
];

fn addr(n: u8) -> H256 {
    H256::repeat_byte(n)
}

fn blast(address: u8, nonce: u64, flags: u8, fixed: u64, shares: u64) -> BlastNewAccount {
    BlastNewAccount {
        address: addr(address),
        nonce,
        flags,
        fixed: U256::from(fixed),
        shares: U256::from(shares),
        remainder: U256::from(nonce),
        code_hash: H256::repeat_byte(0xc0),
    }
}

fn stored(account: &BlastNewAccount) -> StoredAccount {
    StoredAccount::with_ext(
        account.nonce,
        account.code_hash,
        AccountExt::Blast(BlastAccountExt {
            flags: account.flags,
            fixed: account.fixed,
            shares: account.shares,
            remainder: account.remainder,
        }),
    )
}

fn update(accounts: Vec<BlastNewAccount>, deleted: Vec<H256>) -> BlockStateUpdate {
    BlastBlockStorageDiff {
        new_accounts: accounts,
        deleted_accounts: deleted,
        ..Default::default()
    }
    .into()
}

fn block(number: u64) -> BlockInfo {
    let mut header = RawHeader::default();
    header.number = number;
    // A zero hash reads as "no such block", so block n hashes to n + 1.
    header.parent_hash = if number == 0 {
        H256::ZERO
    } else {
        H256::with_last_byte(number as u8)
    };
    BlockInfo::new(Block {
        header: Header {
            hash: H256::with_last_byte(number as u8 + 1),
            inner: header,
            ..Default::default()
        },
        ..Default::default()
    })
}

fn open(name: &str, kind: StorageKind, archive: bool) -> (MultiStorage, PathBuf) {
    set_state_diff_codec(StateDiffCodec::BlastV1);
    set_inverted_block_encoding(false);
    let dir = std::env::temp_dir().join(format!(
        "leafage-blast-{name}-{kind:?}-{archive}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = MultiStorage::open(&dir, 16, kind, archive, false, false).unwrap();
    (db, dir)
}

fn commit(db: &MultiStorage, number: u64, diff: BlockStateUpdate) {
    StateDBWrapper(db.db_at(BlockId::latest()).unwrap().unwrap())
        .update_block(block(number), diff)
        .unwrap();
}

#[test]
fn blast_accounts_round_trip_in_every_backend() {
    let _lock = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (kind, archive) in BACKENDS {
        let (db, dir) = open("round-trip", kind, archive);
        let automatic = blast(1, 1, 0, 0, 13);
        let claimable = blast(2, 2, 2, 42, 5);
        commit(
            &db,
            0,
            update(vec![automatic.clone(), claimable.clone()], vec![]),
        );

        let automatic_1 = blast(1, 3, 0, 0, 20);
        let disabled = blast(3, 4, 1, 7, 0);
        commit(
            &db,
            1,
            update(vec![automatic_1.clone(), disabled.clone()], vec![addr(2)]),
        );

        let latest = db.db_at(BlockId::latest()).unwrap().unwrap();
        let label = format!("{kind:?} archive={archive}");
        assert_eq!(
            latest.read_account(addr(1)).unwrap(),
            Some(stored(&automatic_1)),
            "{label}"
        );
        assert_eq!(latest.read_account(addr(2)).unwrap(), None, "{label}");
        assert_eq!(
            latest.read_account(addr(3)).unwrap(),
            Some(stored(&disabled)),
            "{label}"
        );
        let many = latest
            .read_account_many(&[addr(3), addr(2), addr(1)])
            .unwrap();
        assert_eq!(
            many,
            vec![Some(stored(&disabled)), None, Some(stored(&automatic_1))],
            "{label}"
        );

        let mut tip: Vec<_> = db.account_iter().map(|r| r.unwrap()).collect();
        tip.sort_by_key(|(address, _)| *address);
        assert_eq!(
            tip,
            vec![
                (addr(1), stored(&automatic_1)),
                (addr(3), stored(&disabled))
            ],
            "{label}"
        );

        if archive {
            let at_0 = db.db_at(BlockId::number(0)).unwrap().unwrap();
            assert_eq!(
                at_0.read_account(addr(1)).unwrap(),
                Some(stored(&automatic)),
                "{label}"
            );
            assert_eq!(
                at_0.read_account(addr(2)).unwrap(),
                Some(stored(&claimable)),
                "{label}"
            );
            assert_eq!(at_0.read_account(addr(3)).unwrap(), None, "{label}");
        }
        drop(latest);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Accounts served from uncommitted diff layers, the shared cache and disk
/// all keep their raw fields.
#[test]
fn blast_accounts_through_the_state_tree() {
    let _lock = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (db, dir) = open("state-tree", StorageKind::Rocksdb, false);
    commit(&db, 0, update(vec![blast(1, 1, 0, 0, 13)], vec![]));

    let tree = StateTree::new(db, StateTreeConfig::new(2, 100, 100, 100, true)).unwrap();
    let committed = blast(2, 2, 2, 42, 5);
    let in_memory = blast(3, 3, 0, 0, 8);
    tree.update_block(block(1), update(vec![committed.clone()], vec![]))
        .unwrap();
    tree.update_block(block(2), update(vec![in_memory.clone()], vec![]))
        .unwrap();
    tree.update_block(block(3), update(vec![], vec![])).unwrap();

    let state = tree.state_at(BlockId::latest()).unwrap().unwrap();
    assert_eq!(
        state.raw_account(addr(1)).unwrap(),
        Some(stored(&blast(1, 1, 0, 0, 13)))
    );
    assert_eq!(
        state.raw_account(addr(2)).unwrap(),
        Some(stored(&committed))
    );
    assert_eq!(
        state.raw_account(addr(3)).unwrap(),
        Some(stored(&in_memory))
    );
    assert_eq!(
        state
            .raw_account_many(&[addr(3), addr(4), addr(2)])
            .unwrap(),
        vec![Some(stored(&in_memory)), None, Some(stored(&committed))]
    );
    drop(state);
    drop(tree);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A standard-format record under the Blast codec is a decode error, never a
/// silently misread account.
#[test]
fn standard_records_fail_to_decode_under_the_blast_codec() {
    let _lock = DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (kind, archive) in BACKENDS {
        let (db, dir) = open("mismatch", kind, archive);
        let standard: BlockStorageDiff = BlockStorageDiff {
            new_accounts: vec![NewAccount {
                address: addr(1),
                balance: U256::from(100),
                nonce: 1,
                code_hash: KECCAK256_EMPTY,
            }],
            ..Default::default()
        };
        commit(&db, 0, standard.into());

        let latest = db.db_at(BlockId::latest()).unwrap().unwrap();
        let label = format!("{kind:?} archive={archive}");
        assert!(latest.read_account(addr(1)).is_err(), "{label}");
        assert!(latest.read_account_many(&[addr(1)]).is_err(), "{label}");
        assert!(db.account_iter().any(|r| r.is_err()), "{label}");
        drop(latest);
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
