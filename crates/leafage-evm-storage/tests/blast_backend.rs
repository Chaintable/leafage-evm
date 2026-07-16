//! End-to-end Blast raw-account coverage over all four disk backends.
//!
//! Uses the process-global account codec (`set_account_codec`), so everything
//! lives in a single `#[test]` in its own integration-test binary — its own
//! process — and cannot race other tests over the global. The RocksDB archive
//! backend additionally opens a single DB per process (`DATA_BASE` singleton),
//! so all archive assertions run against the one archive DB of each kind.

use leafage_evm_storage::{
    set_account_codec, EvmStorageWrapper, EvmStorageWrite, LatestStateDBIterator, MultiStorage,
    StateDBProvider, StateDBRead, StateDBWrapper, StorageKind, BLAST_SHARES_HASH,
    SHARE_PRICE_SLOT_HASH,
};
use leafage_evm_types::{
    AccountStorageDiff, AccountUpdate, Address, BalanceState, BlockId, BlockInfo, BlockNumberOrTag,
    BlockStateUpdate, IndexValuePair, StateDiffCodec, StoredAccount, H256, U256,
};
use revm::DatabaseRef;
use std::str::FromStr;

fn block_info(number: u64, hash: H256, parent_hash: H256) -> BlockInfo {
    let mut info = BlockInfo::default();
    info.inner.header.hash = hash;
    info.inner.header.inner.number = number;
    info.inner.header.inner.parent_hash = parent_hash;
    info
}

fn blast_account(shares: u64, remainder: u64) -> StoredAccount {
    StoredAccount {
        nonce: 9,
        code_hash: H256::repeat_byte(0x33),
        balance_state: BalanceState::Blast {
            flags: 0,
            fixed: U256::ZERO,
            shares: U256::from(shares),
            remainder: U256::from(remainder),
        },
    }
}

fn price_diff(account_updates: Vec<AccountUpdate>, price: u64) -> BlockStateUpdate {
    BlockStateUpdate {
        new_accounts: account_updates,
        storage_diffs: vec![AccountStorageDiff {
            address: BLAST_SHARES_HASH,
            diffs: vec![IndexValuePair {
                index: SHARE_PRICE_SLOT_HASH,
                value: U256::from(price),
            }],
        }],
        ..Default::default()
    }
}

/// Fresh latest-view handle. Archive `StateDB` handles snapshot the height at
/// resolve time (production `StateTree::state_at` re-resolves per call), so a
/// handle taken before a write does not see it.
fn latest(
    db: &MultiStorage,
) -> StateDBWrapper<<MultiStorage as StateDBProvider>::StateDBReadWrite> {
    StateDBWrapper(
        db.db_at(BlockId::Number(BlockNumberOrTag::Latest))
            .unwrap()
            .unwrap(),
    )
}

#[test]
fn blast_accounts_roundtrip_all_backends_and_derive_history_correct_balance() {
    set_account_codec(StateDiffCodec::BlastV1);

    let address = Address::from_str("0x455875815af7E846317D9E73e9Ea65d19EC58A82").unwrap();
    let addr_hash: H256 = alloy::primitives::keccak256(address.as_slice());
    let account = blast_account(13, 17);
    let account_update = AccountUpdate {
        address: addr_hash,
        account: account.clone(),
    };
    let p1 = 1_000_000_000u64;
    let p2 = 1_019_184_352u64;

    let base = std::env::temp_dir().join(format!(
        "leafage-blast-backend-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));

    let mut last_db = None;
    for (name, kind, archive) in [
        ("rocksdb-state", StorageKind::Rocksdb, false),
        ("rocksdb-archive", StorageKind::Rocksdb, true),
        ("mdbx-state", StorageKind::MDBX, false),
        ("mdbx-archive", StorageKind::MDBX, true),
    ] {
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        let db = MultiStorage::open(&path, 64, kind, archive, false, false).unwrap();

        // Block 1: the Blast account is written together with sharePrice p1.
        latest(&db)
            .update_block(
                block_info(1, H256::repeat_byte(0x11), H256::ZERO),
                price_diff(vec![account_update.clone()], p1),
            )
            .unwrap();

        let read = latest(&db).0.read_account(addr_hash).unwrap().unwrap();
        assert_eq!(read, account, "{name}: read_account round-trip");
        let iterated: Vec<_> = db.account_iter().map(|r| r.unwrap()).collect();
        assert_eq!(
            iterated,
            vec![(addr_hash, account.clone())],
            "{name}: account_iter"
        );

        // Block 2: only the sharePrice moves; the account row is untouched.
        latest(&db)
            .update_block(
                block_info(2, H256::repeat_byte(0x12), H256::repeat_byte(0x11)),
                price_diff(vec![], p2),
            )
            .unwrap();

        // Idle-but-rebased at the latest view: same account row, new price.
        let wrapper = EvmStorageWrapper {
            db: latest(&db),
            ovm_address: None,
            normalize_state_key: false,
        };
        assert_eq!(
            wrapper.basic_ref(address).unwrap().unwrap().balance,
            U256::from(13 * p2 + 17),
            "{name}: latest balance uses p2"
        );

        // Historical views (archive backends only): the same account row must
        // materialize different balances at the two heights.
        if archive {
            for (height, price) in [(1u64, p1), (2u64, p2)] {
                let view = StateDBWrapper(
                    db.db_at(BlockId::Number(BlockNumberOrTag::Number(height)))
                        .unwrap()
                        .unwrap(),
                );
                let wrapper = EvmStorageWrapper {
                    db: view,
                    ovm_address: None,
                    normalize_state_key: false,
                };
                assert_eq!(
                    wrapper.basic_ref(address).unwrap().unwrap().balance,
                    U256::from(13 * price + 17),
                    "{name}: balance at height {height}"
                );
            }
        }

        // Block 3: deletion round-trips (snapshot: key delete; archive: empty
        // sentinel).
        let mut delete_diff = BlockStateUpdate::default();
        delete_diff.deleted_accounts.push(addr_hash);
        latest(&db)
            .update_block(
                block_info(3, H256::repeat_byte(0x13), H256::repeat_byte(0x12)),
                delete_diff,
            )
            .unwrap();
        assert!(
            latest(&db).0.read_account(addr_hash).unwrap().is_none(),
            "{name}: deleted"
        );

        last_db = Some(db);
    }

    // Strict codec: reading a blast-v1 DB under the standard codec must error
    // on every account read — never silently reinterpret 6-item records. The
    // account was deleted at block 3, so read a pre-delete height (archive).
    set_account_codec(StateDiffCodec::Standard);
    let db = last_db.unwrap();
    let view = StateDBWrapper(
        db.db_at(BlockId::Number(BlockNumberOrTag::Number(1)))
            .unwrap()
            .unwrap(),
    );
    assert!(view.0.read_account(addr_hash).is_err());

    let _ = std::fs::remove_dir_all(&base);
}
