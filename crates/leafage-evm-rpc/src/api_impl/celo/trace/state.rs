//! Celo's StateDB keeps deleted objects and pending trie writes across txs.
//! SubBalance(address, 0) can recreate a deleted caller without dirtying it:
//! resetObjectChange.dirtied() is nil, unlike createObjectChange. A pending
//! write then saves that empty object. Randomness flushes pending writes via
//! IntermediateRoot; primary transactions only call Finalise.
use crate::pipeline::changes::RecordingDb;
use alloy::primitives::{Address, B256, U256};
use revm::{
    database::CacheDB,
    state::{AccountInfo, EvmState},
    Database, DatabaseCommit, DatabaseRef,
};
use std::{
    collections::HashSet,
    ops::{Deref, DerefMut},
};

pub(super) struct CeloBlockDb<D: DatabaseRef> {
    inner: RecordingDb<D>,
    deleted: HashSet<Address>,
    pending: HashSet<Address>,
}

impl<D: DatabaseRef> CeloBlockDb<D> {
    pub fn new(db: CacheDB<D>, track: bool) -> Self {
        Self {
            inner: RecordingDb::new(db, track, false),
            deleted: HashSet::new(),
            pending: HashSet::new(),
        }
    }

    /// Native IntermediateRoot flushes pending writes, but retains objects.
    pub fn intermediate_root(&mut self) {
        self.pending.clear();
    }
}

impl<D: DatabaseRef> Deref for CeloBlockDb<D> {
    type Target = RecordingDb<D>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl<D: DatabaseRef> DerefMut for CeloBlockDb<D> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
impl<D: DatabaseRef> Database for CeloBlockDb<D> {
    type Error = D::Error;
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.inner.basic(address)
    }
    fn code_by_hash(&mut self, hash: B256) -> Result<revm::bytecode::Bytecode, Self::Error> {
        self.inner.code_by_hash(hash)
    }
    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.inner.storage(address, index)
    }
    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash(number)
    }
}

impl<D: DatabaseRef> DatabaseCommit for CeloBlockDb<D> {
    fn commit(&mut self, mut state: EvmState) {
        let mut memory_only = EvmState::default();
        state.retain(|address, account| {
            // An implicit zero-value caller creation has no revm touch. Real
            // CREATE and balance/storage changes are touched independently.
            if account.is_created() && !account.is_touched() && !account.is_selfdestructed() {
                account.mark_touch(); // let CacheDB save the recreated object
                if self.deleted.remove(address) {
                    if !self.pending.contains(address) {
                        // Still visible to subsequent execution, but no trie write.
                        memory_only.insert(*address, account.clone());
                        return false;
                    }
                    return true; // pending reset survives EIP-161
                }
                // First creation dirties the native object, even with zero value.
            }
            if account.is_touched() {
                self.pending.insert(*address);
                if account.is_empty() {
                    account.mark_selfdestruct();
                }
                if account.is_selfdestructed() {
                    self.deleted.insert(*address);
                } else {
                    self.deleted.remove(address);
                }
            }
            true
        });
        self.inner.commit(state);
        self.inner.db.commit(memory_only);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_chains::celo::l1::{
        consensus::{CeloChainData, CeloHeader},
        evm::{create_celo_l1_evm, system_call_inspected},
        L1Fork,
    };
    use revm::{
        bytecode::Bytecode, context::BlockEnv, context_interface::JournalTr, database::EmptyDB,
        inspector::NoOpInspector, primitives::Bytes, ExecuteEvm,
    };

    struct NoChain;
    impl CeloChainData for NoChain {
        fn header(&self, _: u64) -> Result<CeloHeader, String> {
            Err("unexpected header read".into())
        }
    }
    const CALLER: Address = Address::ZERO;
    const CONTRACT: Address = Address::repeat_byte(0x11);
    type Db = CeloBlockDb<EmptyDB>;

    fn db() -> Db {
        let mut db = Db::new(CacheDB::new(EmptyDB::default()), true);
        db.db.insert_account_info(
            CONTRACT,
            AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&[0x00]))),
        );
        db
    }
    fn call(db: &mut Db, to: Address, is_static: bool, revert_outer: bool) {
        let mut evm = create_celo_l1_evm(
            BlockEnv::default(),
            super::super::replay::config(),
            db,
            NoOpInspector,
            &NoChain,
        )
        .unwrap();
        evm.inner
            .ctx
            .journaled_state
            .set_spec_id(L1Fork::Istanbul.spec());
        let checkpoint = evm.inner.ctx.journaled_state.checkpoint();
        let result = system_call_inspected(
            &mut evm.inner.ctx,
            L1Fork::Istanbul,
            &NoChain,
            CALLER,
            to,
            Bytes::new(),
            100_000,
            is_static,
            false,
            None,
        )
        .unwrap();
        assert!(result.result.is_ok() || result.result.is_revert());
        if revert_outer {
            evm.inner.ctx.journaled_state.checkpoint_revert(checkpoint);
        } else {
            evm.inner.ctx.journaled_state.checkpoint_commit();
        }
        let state = evm.finalize();
        evm.inner.ctx.journaled_state.database.commit(state);
    }
    fn archive(db: &Db, exists: bool) {
        let mut post = db.db.clone();
        if exists {
            post.insert_account_info(CALLER, AccountInfo::default());
        } else {
            post.cache.accounts.remove(&CALLER);
        }
        db.changes.as_ref().unwrap().verify(&post).unwrap();
        let diff = db
            .changes
            .as_ref()
            .unwrap()
            .to_storage_diff(&db.db, None, B256::ZERO, B256::ZERO)
            .unwrap();
        assert_eq!(
            diff.new_accounts
                .iter()
                .any(|a| a.address == alloy::primitives::keccak256(CALLER)),
            exists
        );
    }

    #[test]
    fn empty_caller_reset_obeys_native_pending_trie_writes() {
        // Native StateDB oracle: SubBalance(0); IntermediateRoot(true);
        // [AddBalance(0); Finalise(true);] SubBalance(0); Commit(true).
        // The empty account is persisted iff the bracketed touch occurred.
        for touch in [false, true] {
            let mut db = db();
            call(&mut db, CONTRACT, false, false);
            db.intermediate_root();
            assert!(db.basic(CALLER).unwrap().is_none());
            if touch {
                call(&mut db, CALLER, true, false);
            }
            call(&mut db, CONTRACT, false, false);
            assert!(db.basic(CALLER).unwrap().is_some()); // native in-memory reset
            archive(&db, touch);
        }
    }

    #[test]
    fn caller_creation_and_reset_follow_call_and_outer_reverts() {
        for outer in [false, true] {
            let mut db = db();
            call(&mut db, CALLER, true, false);
            if !outer {
                db.db.insert_account_info(
                    CONTRACT,
                    AccountInfo::default().with_code(Bytecode::new_raw(Bytes::from_static(&[
                        0x60, 0x00, 0x60, 0x00, 0xfd,
                    ]))),
                );
            }
            call(&mut db, CONTRACT, false, outer);
            assert!(db.basic(CALLER).unwrap().is_none());
            archive(&db, false);
        }
    }

    #[test]
    fn missing_destination_call_does_not_create_pending_deletion() {
        let mut db = db();
        let missing = Address::repeat_byte(0x22);
        call(&mut db, missing, false, false);
        call(&mut db, CALLER, false, false);
        assert!(db.pending.is_empty());
        assert!(db.deleted.is_empty());
        assert!(db.basic(CALLER).unwrap().is_none());
        call(&mut db, CONTRACT, false, false);
        archive(&db, false);
    }

    #[test]
    fn a_later_touch_deletes_a_recreated_empty_caller() {
        let mut db = db();
        call(&mut db, CONTRACT, false, false);
        call(&mut db, CONTRACT, false, false);
        archive(&db, true);
        call(&mut db, CALLER, true, false);
        assert!(db.basic(CALLER).unwrap().is_none());
        archive(&db, false);
    }
}
