use revm::context::journal::inner::JournalInner;
use revm::context_interface::journaled_state::entry::SelfdestructionRevertStatus;
use revm::primitives::{Address, KECCAK_EMPTY, U256};
use revm::JournalEntry;
use std::collections::{BTreeMap, BTreeSet};

const ACCOUNT_IDX_KEY_SIZE: usize = 24;
const ACCOUNT_IDX_SIZE: usize = 8;
const DB_ACCOUNT_SIZE_EOA: usize = 41;
const DB_ACCOUNT_SIZE_CONTRACT: usize = 73;
const DB_ACCOUNT_KEY_SIZE: usize = 12;
const STORAGE_KEY_SIZE: usize = 36;
const STORAGE_VALUE_SIZE: usize = 32;

pub const L1_FEE_OVERHEAD: usize = 2;
pub const BROTLI_COMPRESSION_PERCENTAGE: usize = 48;
const STORAGE_DISCOUNTED_PERCENTAGE: usize = 66;
const ACCOUNT_DISCOUNTED_PERCENTAGE: usize = 32;

pub fn calc_diff_size(journal_inner: &JournalInner<JournalEntry>, caller: &Address) -> usize {
    let journal = journal_inner.journal.iter();
    let state = &journal_inner.state;

    #[derive(Default)]
    struct AccountChange {
        storage_changes: BTreeSet<U256>,
        account_info_changed: bool,
    }

    let mut account_changes: BTreeMap<Address, AccountChange> = BTreeMap::new();

    account_changes
        .entry(*caller)
        .or_default()
        .account_info_changed = true;

    for entry in journal {
        match entry {
            JournalEntry::NonceChange { address, .. } => {
                account_changes
                    .entry(address.clone())
                    .or_default()
                    .account_info_changed = true;
            }
            JournalEntry::BalanceTransfer { from, to, .. } => {
                account_changes
                    .entry(from.clone())
                    .or_default()
                    .account_info_changed = true;
                account_changes
                    .entry(to.clone())
                    .or_default()
                    .account_info_changed = true;
            }
            JournalEntry::StorageChanged { address, key, .. } => {
                account_changes
                    .entry(address.clone())
                    .or_default()
                    .storage_changes
                    .insert(*key);
            }
            JournalEntry::CodeChange { address } => {
                account_changes
                    .entry(address.clone())
                    .or_default()
                    .account_info_changed = true;
            }
            JournalEntry::AccountCreated { address, .. } => {
                account_changes
                    .entry(address.clone())
                    .or_default()
                    .account_info_changed = true;
            }
            JournalEntry::AccountDestroyed {
                address,
                target,
                destroyed_status,
                had_balance,
            } => {
                if matches!(
                    destroyed_status,
                    SelfdestructionRevertStatus::RepeatedSelfdestruction
                ) {
                    continue;
                }
                if address != target && !had_balance.is_zero() {
                    account_changes
                        .entry(target.clone())
                        .or_default()
                        .account_info_changed = true;
                }
            }
            _ => {}
        }
    }

    let mut account_based_diff = 0usize;
    let mut storage_based_diff = 0usize;
    let mut new_account_based_diff = 0usize;

    for (addr, account) in &account_changes {
        if account.account_info_changed {
            let db_account_size = {
                let acct = &state[addr];
                if acct.info.code_hash == KECCAK_EMPTY {
                    DB_ACCOUNT_SIZE_EOA
                } else {
                    DB_ACCOUNT_SIZE_CONTRACT
                }
            };
            account_based_diff += db_account_size + DB_ACCOUNT_KEY_SIZE;
        }

        let slot_size = STORAGE_KEY_SIZE + STORAGE_VALUE_SIZE;
        storage_based_diff += slot_size * account.storage_changes.len();

        if let Some(acct) = state.get(addr) {
            if !acct.is_loaded_as_not_existing() && acct.is_created() {
                new_account_based_diff += ACCOUNT_IDX_KEY_SIZE + ACCOUNT_IDX_SIZE;
            }
        }
    }

    (account_based_diff * ACCOUNT_DISCOUNTED_PERCENTAGE / 100)
        + (storage_based_diff * STORAGE_DISCOUNTED_PERCENTAGE / 100)
        + new_account_based_diff
}
