//! The state changes of a replayed block, with each account's and slot's
//! value before the block: checked against the archive state of the block,
//! and turned into the block's state diff.
//!
//! The values before the block are taken when an account is first changed,
//! from the replay's [`CacheDB`]: the execution has just read it, so no
//! extra archive read is needed.

use alloy::primitives::{keccak256, Address, B256, U256};
use anyhow::{bail, Result};
use leafage_evm_storage::{EvmStorageRead, EvmStorageWrapper, StateDB};
use leafage_evm_types::BlockId;
use leafage_evm_types::{
    AccountStorageDiff, BlockStorageDiff, IndexValuePair, NewAccount, NewCode,
};
use revm::database::CacheDB;
use revm::primitives::KECCAK_EMPTY;
use revm::state::{AccountInfo, EvmState};
use revm::{Database, DatabaseCommit, DatabaseRef};
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// Mismatches listed in a verification error before it is cut short.
const MAX_REPORTED_MISMATCHES: usize = 20;

/// A state that can list an account's storage.
pub(crate) trait StorageSlots {
    /// The slots of `address` holding a non-zero value, hashed keys with
    /// the value, or `None` if the state cannot list them.
    fn storage_slots(&self, address: Address) -> Result<Option<Vec<(B256, U256)>>>;
}

impl<T> StorageSlots for EvmStorageWrapper<T>
where
    T: StateDB,
    T::Error: Send + Sync + 'static,
{
    fn storage_slots(&self, address: Address) -> Result<Option<Vec<(B256, U256)>>> {
        Ok(self.storage_slots_ref(address)?)
    }
}

/// The archive state of a block, opened when first listed: few blocks
/// destroy an account.
pub(crate) struct LazyState<DB: EvmStorageRead> {
    db: Arc<DB>,
    hash: B256,
    state: OnceCell<EvmStorageWrapper<DB::StateDB>>,
}

impl<DB: EvmStorageRead> LazyState<DB> {
    pub(crate) fn new(db: Arc<DB>, hash: B256) -> Self {
        Self {
            db,
            hash,
            state: OnceCell::new(),
        }
    }
}

impl<DB: EvmStorageRead> StorageSlots for LazyState<DB> {
    fn storage_slots(&self, address: Address) -> Result<Option<Vec<(B256, U256)>>> {
        let state = match self.state.get() {
            Some(state) => state,
            None => {
                let hash = self.hash;
                let state = self
                    .db
                    .state_at(BlockId::Hash(hash.into()))?
                    .ok_or_else(|| {
                        anyhow::anyhow!("the state of block {hash} is not in the archive")
                    })?;
                self.state.get_or_init(|| EvmStorageWrapper {
                    db: state,
                    ovm_address: None,
                    normalize_state_key: false,
                })
            }
        };
        state.storage_slots(address)
    }
}

/// Accounts and slots written by the replay, with their values before and
/// after the block.
#[derive(Default)]
pub(crate) struct ChangeSet {
    accounts: HashMap<Address, AccountChange>,
    /// Empty accounts exist: Ethereum before Spurious Dragon creates them,
    /// and leaves them in the state until touched. Otherwise (BSC, EIP-161
    /// from genesis) an empty account counts as absent.
    keep_empty: bool,
}

pub(crate) struct AccountChange {
    /// Before the block; `None` if the account did not exist (or was empty).
    original: Option<AccountInfo>,
    /// `None` once the account was destroyed.
    pub info: Option<AccountInfo>,
    storage: HashMap<U256, SlotChange>,
    /// Destroyed during the block, whether or not it exists again by its end.
    destroyed: bool,
}

struct SlotChange {
    original: U256,
    present: U256,
}

impl ChangeSet {
    /// A change set that tells empty accounts from absent ones.
    pub(crate) fn keeping_empty() -> Self {
        Self {
            accounts: HashMap::new(),
            keep_empty: true,
        }
    }

    /// `info` if it counts as an existing account.
    fn live<'a>(&self, info: Option<&'a AccountInfo>) -> Option<&'a AccountInfo> {
        info.filter(|info| self.keep_empty || !is_empty(info))
    }

    /// The change entry of `address`, created with the value the account
    /// had before the block. Called before the first write to it, so `db`
    /// still holds that value, usually cached by the execution.
    pub(crate) fn track<D: DatabaseRef>(
        &mut self,
        db: &mut CacheDB<D>,
        address: Address,
    ) -> Result<&mut AccountChange, D::Error> {
        if !self.accounts.contains_key(&address) {
            let original = db.basic(address)?;
            let original = self.live(original.as_ref()).cloned();
            self.accounts.insert(
                address,
                AccountChange {
                    info: original.clone(),
                    original,
                    storage: HashMap::new(),
                    destroyed: false,
                },
            );
        }
        Ok(self.accounts.get_mut(&address).expect("inserted above"))
    }

    /// Fold the state changes of a transaction (or system call, or balance
    /// change) into the block's, before they are committed to `db`.
    pub(crate) fn record<D: DatabaseRef>(
        &mut self,
        db: &mut CacheDB<D>,
        state: &EvmState,
    ) -> Result<(), D::Error> {
        for (address, account) in state {
            if !account.is_touched() {
                continue;
            }
            let change = self.track(db, *address)?;
            if account.is_selfdestructed() || account.is_created() {
                for slot in change.storage.values_mut() {
                    slot.present = U256::ZERO;
                }
            }
            let destroyed = account.is_selfdestructed();
            change.destroyed |= destroyed;
            change.info = (!destroyed).then(|| account.info.clone());
            for (key, value) in &account.storage {
                // A destroyed account's slots, written in the transaction
                // or not, are gone.
                if value.is_changed() || destroyed {
                    change
                        .storage
                        .entry(*key)
                        .or_insert(SlotChange {
                            original: value.original_value(),
                            present: U256::ZERO,
                        })
                        .present = if destroyed {
                        U256::ZERO
                    } else {
                        value.present_value()
                    };
                }
            }
        }
        Ok(())
    }

    /// Record a write of `value` to `slot` of `address` made outside the
    /// EVM; `original` is the slot before the write.
    pub(crate) fn set_slot(change: &mut AccountChange, slot: U256, original: U256, value: U256) {
        change
            .storage
            .entry(slot)
            .or_insert(SlotChange {
                original,
                present: U256::ZERO,
            })
            .present = value;
    }

    /// Compare every changed account and slot with the archive state of the
    /// block.
    pub(crate) fn verify<D>(&self, post: &D) -> Result<()>
    where
        D: DatabaseRef,
        D::Error: Send + Sync + 'static,
    {
        let mut mismatches = Vec::new();
        let mut addresses: Vec<_> = self.accounts.keys().collect();
        addresses.sort();
        for address in addresses {
            let change = &self.accounts[address];
            let ours = self.live(change.info.as_ref());
            let stored = post.basic_ref(*address)?;
            let stored = self.live(stored.as_ref());
            match (ours, stored) {
                (None, None) => {}
                (Some(ours), Some(stored))
                    if ours.balance == stored.balance
                        && ours.nonce == stored.nonce
                        && normalize_code_hash(ours.code_hash)
                            == normalize_code_hash(stored.code_hash) => {}
                (ours, stored) => mismatches.push(format!(
                    "account {address}: replay {}, archive {}",
                    describe(ours),
                    describe(stored)
                )),
            }
            let Some(ours) = ours else { continue };
            // A contract created and destroyed within the block, then touched
            // into an empty account (before Spurious Dragon), may keep the
            // slots its constructor wrote in the archive. Without code no one
            // reads them, so only the slots the replay left non-zero count.
            let unreadable =
                change.destroyed && normalize_code_hash(ours.code_hash) == KECCAK_EMPTY;
            let mut slots: Vec<_> = change
                .storage
                .iter()
                .map(|(slot, value)| (slot, &value.present))
                .collect();
            slots.sort();
            for (slot, value) in slots {
                if unreadable && value.is_zero() {
                    continue;
                }
                let stored = post.storage_ref(*address, *slot)?;
                if stored != *value {
                    mismatches.push(format!(
                        "storage {address}[{slot:#x}]: replay {value:#x}, archive {stored:#x}"
                    ));
                }
            }
        }
        if mismatches.is_empty() {
            return Ok(());
        }
        let total = mismatches.len();
        mismatches.truncate(MAX_REPORTED_MISMATCHES);
        bail!(
            "replayed state differs from the archive in {total} places: {}",
            mismatches.join("; ")
        )
    }

    /// The `BlockStorageDiff` of the block in the leafage / pipeline
    /// encoding, with net-change semantics: an account is listed only when
    /// its balance, nonce or code changed, a slot only when its final value
    /// differs from the one before the block.
    /// `parent_state`, the state before the block, lists the storage the
    /// accounts the block destroys lost; without it only the slots the
    /// block touched are written out as zero.
    pub(crate) fn to_storage_diff<D>(
        &self,
        db: &CacheDB<D>,
        parent_state: Option<&dyn StorageSlots>,
        state_root: B256,
        parent_state_root: B256,
    ) -> Result<BlockStorageDiff>
    where
        D: DatabaseRef,
        D::Error: Send + Sync + 'static,
    {
        let mut new_accounts = BTreeMap::new();
        let mut deleted_accounts = BTreeSet::new();
        let mut storage_diffs = BTreeMap::new();
        let mut new_codes = BTreeMap::new();
        for (address, change) in &self.accounts {
            let hashed = keccak256(address);
            let before = change.original.as_ref();
            // A destroyed account's slots set before the block are zero now,
            // also when it is created again, like geth lists them: a reader
            // of the diff that does not clear the storage of deleted
            // accounts would keep them otherwise, and a contract created
            // again within the block is not listed as deleted at all.
            let slots_before = match parent_state {
                Some(state) if change.destroyed && before.is_some() => state
                    .storage_slots(*address)?
                    .map(|slots| slots.into_iter().collect::<HashMap<_, _>>()),
                _ => None,
            };
            if let Some(diffs) = storage_diff(hashed, &change.storage, slots_before.as_ref()) {
                storage_diffs.insert(hashed, diffs);
            }
            let Some(info) = self.live(change.info.as_ref()) else {
                if before.is_some() {
                    deleted_accounts.insert(hashed);
                }
                continue;
            };
            let unchanged = before.is_some_and(|before| {
                before.balance == info.balance
                    && before.nonce == info.nonce
                    && normalize_code_hash(before.code_hash) == normalize_code_hash(info.code_hash)
            });
            if !unchanged {
                new_accounts.insert(
                    hashed,
                    NewAccount {
                        address: hashed,
                        balance: info.balance,
                        nonce: info.nonce,
                        code_hash: info.code_hash,
                    },
                );
            }
            let code_hash = normalize_code_hash(info.code_hash);
            let code_before = before.map(|before| normalize_code_hash(before.code_hash));
            if code_hash != KECCAK_EMPTY && code_before != Some(code_hash) {
                let code = db.code_by_hash_ref(info.code_hash)?;
                new_codes.insert(
                    code_hash,
                    NewCode {
                        code_hash,
                        code: code.original_bytes(),
                    },
                );
            }
        }
        Ok(BlockStorageDiff {
            hash: state_root,
            parent_hash: parent_state_root,
            new_accounts: new_accounts.into_values().collect(),
            deleted_accounts: deleted_accounts.into_iter().collect(),
            storage_diffs: storage_diffs.into_values().collect(),
            new_codes: new_codes.into_values().collect(),
        })
    }
}

/// The slots of `storage` whose value differs from the one before the block.
/// The slots of `storage` that changed. `before`, the account's non-zero
/// slots (hashed keys) before the block, is given for an account the block
/// destroyed: a slot first written after the destruction recorded the
/// cleared storage as its original value, and the slots the block did not
/// touch are zero now.
fn storage_diff(
    hashed: B256,
    storage: &HashMap<U256, SlotChange>,
    before: Option<&HashMap<B256, U256>>,
) -> Option<AccountStorageDiff> {
    let mut touched = HashSet::new();
    let mut diffs = Vec::new();
    for (key, slot) in storage {
        let index = keccak256(key.to_be_bytes::<32>());
        let original = match before {
            Some(before) => before.get(&index).copied().unwrap_or_default(),
            None => slot.original,
        };
        if slot.present != original {
            diffs.push(IndexValuePair {
                index,
                value: slot.present,
            });
        }
        touched.insert(index);
    }
    for index in before.into_iter().flat_map(|before| before.keys()) {
        if !touched.contains(index) {
            diffs.push(IndexValuePair {
                index: *index,
                value: U256::ZERO,
            });
        }
    }
    if diffs.is_empty() {
        return None;
    }
    diffs.sort_by_key(|pair| pair.index);
    Some(AccountStorageDiff {
        address: hashed,
        diffs,
    })
}

/// A [`CacheDB`] that records what is committed to it in a [`ChangeSet`]
/// and applies EIP-161, for executors that commit on their own (system
/// calls, balance increments).
pub(crate) struct RecordingDb<D: DatabaseRef> {
    pub db: CacheDB<D>,
    /// `None` when the changes are not needed.
    pub changes: Option<ChangeSet>,
    /// EIP-161: delete the touched accounts left empty.
    pub state_clear: bool,
    /// The first error recording a commit hit; `commit` cannot return it.
    pub error: Option<D::Error>,
}

impl<D: DatabaseRef> RecordingDb<D> {
    /// Empty accounts count as existing (see [`ChangeSet::keeping_empty`]).
    pub(crate) fn new(db: CacheDB<D>, track: bool, state_clear: bool) -> Self {
        Self {
            db,
            changes: track.then(ChangeSet::keeping_empty),
            state_clear,
            error: None,
        }
    }
}

impl<D: DatabaseRef> std::fmt::Debug for RecordingDb<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingDb")
            .field("state_clear", &self.state_clear)
            .finish_non_exhaustive()
    }
}

impl<D: DatabaseRef> Database for RecordingDb<D> {
    type Error = D::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.db.basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<revm::bytecode::Bytecode, Self::Error> {
        self.db.code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.db.storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db.block_hash(number)
    }
}

impl<D: DatabaseRef> DatabaseCommit for RecordingDb<D> {
    fn commit(&mut self, mut state: EvmState) {
        // Clear first, so the record sees the deletions.
        if self.state_clear {
            delete_touched_empty(&mut state);
        }
        if let Some(changes) = &mut self.changes {
            if let Err(err) = changes.record(&mut self.db, &state) {
                self.error.get_or_insert(err);
            }
        }
        self.db.commit(state);
    }
}

/// EIP-161: a touched account left empty is deleted. CacheDB would keep it
/// as an existing empty account, which EIP-7702 refunds as an existing
/// authority.
pub(crate) fn delete_touched_empty(state: &mut EvmState) {
    for account in state.values_mut() {
        if account.is_touched() && account.is_empty() {
            account.mark_selfdestruct();
        }
    }
}

/// EIP-161 empty: such accounts are removed from the trie.
pub(crate) fn is_empty(info: &AccountInfo) -> bool {
    info.balance.is_zero() && info.nonce == 0 && normalize_code_hash(info.code_hash) == KECCAK_EMPTY
}

pub(crate) fn normalize_code_hash(code_hash: B256) -> B256 {
    if code_hash.is_zero() {
        KECCAK_EMPTY
    } else {
        code_hash
    }
}

fn describe(info: Option<&AccountInfo>) -> String {
    match info {
        None => "absent".to_string(),
        Some(info) => format!(
            "balance {} nonce {} code {}",
            info.balance, info.nonce, info.code_hash
        ),
    }
}

/// Fail on a failed check, or only log it when not `strict`.
pub(crate) fn check(strict: bool, result: Result<()>) -> Result<()> {
    match result {
        Err(err) if !strict => {
            tracing::warn!("ignoring failed check: {err:#}");
            Ok(())
        }
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touched_empty_accounts_are_deleted() {
        use revm::database::EmptyDB;
        use revm::state::Account;
        let (empty, funded) = (Address::with_last_byte(1), Address::with_last_byte(2));
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(empty, AccountInfo::default());
        let mut state = EvmState::default();
        for (address, balance) in [(empty, 0), (funded, 1)] {
            let mut account = Account::from(AccountInfo {
                balance: U256::from(balance),
                ..Default::default()
            });
            account.mark_touch();
            state.insert(address, account);
        }
        delete_touched_empty(&mut state);
        db.commit(state);
        assert_eq!(db.basic(empty).unwrap(), None);
        assert_eq!(db.basic(funded).unwrap().unwrap().balance, U256::from(1));
    }

    /// A parent state holding `slots` (index, value) in every account.
    struct Slots(Vec<(u64, u64)>);

    impl StorageSlots for Slots {
        fn storage_slots(&self, _address: Address) -> Result<Option<Vec<(B256, U256)>>> {
            Ok(Some(
                self.0
                    .iter()
                    .map(|(slot, value)| {
                        (
                            keccak256(U256::from(*slot).to_be_bytes::<32>()),
                            U256::from(*value),
                        )
                    })
                    .collect(),
            ))
        }
    }

    fn pair(slot: u64, value: u64) -> IndexValuePair {
        IndexValuePair {
            index: keccak256(U256::from(slot).to_be_bytes::<32>()),
            value: U256::from(value),
        }
    }

    /// A contract with slots 1 = 7 and 5 = 8 before the block, destroyed by
    /// a transaction that writes slots 1 and 2 first. With `again`, later
    /// transactions create it again and make the writes `(slot, from, to)`,
    /// one transaction each.
    fn destroy(
        again: Option<&[(u64, u64, u64)]>,
        parent: Option<&dyn StorageSlots>,
    ) -> BlockStorageDiff {
        use revm::database::EmptyDB;
        use revm::state::{Account, EvmStorageSlot, TransactionId};
        let contract = Address::with_last_byte(9);
        let mut db = CacheDB::new(EmptyDB::default());
        let info = AccountInfo {
            nonce: 1,
            ..Default::default()
        };
        db.insert_account_info(contract, info.clone());
        for (slot, value) in [(1u64, 7u64), (5, 8)] {
            db.insert_account_storage(contract, U256::from(slot), U256::from(value))
                .unwrap();
        }
        let slot = |from: u64, to: u64| {
            EvmStorageSlot::new_changed(U256::from(from), U256::from(to), TransactionId::ZERO)
        };
        let mut changes = ChangeSet::keeping_empty();
        let mut commit = |account: Account, changes: &mut ChangeSet| {
            let state = EvmState::from_iter([(contract, account)]);
            changes.record(&mut db, &state).unwrap();
            db.commit(state);
        };
        let mut account = Account::from(info.clone());
        account.mark_touch();
        account.mark_selfdestruct();
        account.storage.insert(U256::from(1), slot(7, 3));
        account.storage.insert(U256::from(2), slot(0, 3));
        commit(account, &mut changes);
        for (index, (key, from, to)) in again.unwrap_or_default().iter().enumerate() {
            let mut account = Account::from(info.clone());
            account.mark_touch();
            if index == 0 {
                account.mark_created();
            }
            account.storage.insert(U256::from(*key), slot(*from, *to));
            commit(account, &mut changes);
        }
        changes
            .to_storage_diff(&db, parent, B256::ZERO, B256::ZERO)
            .unwrap()
    }

    fn sorted(mut pairs: Vec<IndexValuePair>) -> Vec<IndexValuePair> {
        pairs.sort_by_key(|pair| pair.index);
        pairs
    }

    #[test]
    fn destroyed_account_lists_its_set_slots_as_zero() {
        let hashed = keccak256(Address::with_last_byte(9));
        // Without the parent state, only the slots the block touched.
        let diff = destroy(None, None);
        assert_eq!(diff.deleted_accounts, vec![hashed]);
        assert!(diff.new_accounts.is_empty());
        assert_eq!(diff.storage_diffs.len(), 1);
        assert_eq!(diff.storage_diffs[0].diffs, vec![pair(1, 0)]);

        let parent = Slots(vec![(1, 7), (5, 8)]);
        let diff = destroy(None, Some(&parent));
        assert_eq!(diff.deleted_accounts, vec![hashed]);
        assert_eq!(
            diff.storage_diffs[0].diffs,
            sorted(vec![pair(1, 0), pair(5, 0)])
        );

        // Created again: not deleted, and still loses slots 1 and 5.
        let diff = destroy(Some(&[(2, 0, 4)]), Some(&parent));
        assert!(diff.deleted_accounts.is_empty());
        assert_eq!(
            diff.storage_diffs[0].diffs,
            sorted(vec![pair(1, 0), pair(2, 4), pair(5, 0)])
        );
    }

    /// A slot first written after the destruction compares with its value
    /// before the block, not with the cleared storage.
    #[test]
    fn slot_written_after_destruction_compares_with_the_parent() {
        let parent = Slots(vec![(1, 7), (5, 8)]);
        // Slot 5 set to 3 and back to 0: lost all the same.
        let diff = destroy(Some(&[(5, 0, 3), (5, 3, 0)]), Some(&parent));
        assert_eq!(
            diff.storage_diffs[0].diffs,
            sorted(vec![pair(1, 0), pair(5, 0)])
        );
        // Slot 5 set to 8 again: unchanged.
        let diff = destroy(Some(&[(5, 0, 8)]), Some(&parent));
        assert_eq!(diff.storage_diffs[0].diffs, vec![pair(1, 0)]);
    }
}
