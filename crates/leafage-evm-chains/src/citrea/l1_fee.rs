//! L1 fee calculation for Citrea DA (data availability) costs.
//!
//! Walks EVM journal entries to estimate the state diff size produced by a
//! transaction, then applies brotli-like compression ratios to derive the
//! L1 data posting cost.

use std::collections::{BTreeMap, BTreeSet};

use leafage_evm_types::Address;
use revm::context_interface::journaled_state::entry::JournalEntry;
use revm::primitives::KECCAK_EMPTY;

// ── Compression / sizing constants ──────────────────────────────────

/// Brotli compression ratio numerator (48%).
pub(crate) const BROTLI_COMPRESSION_PERCENTAGE: u64 = 48;

/// Fixed overhead added after compression (bytes).
pub(crate) const BROTLI_EXTRA_BYTES: u64 = 2;

/// Weight for account-info diffs in the final size calculation (32%).
const ACCOUNT_DIFF_WEIGHT: u64 = 32;

/// Weight for storage diffs in the final size calculation (66%).
const STORAGE_DIFF_WEIGHT: u64 = 66;

/// Byte size of a single storage key+value diff entry (36-byte key + 32-byte value).
const STORAGE_ENTRY_SIZE: u64 = 68;

/// Byte size of an account-info diff for accounts without code (nonce + balance + flags).
const ACCOUNT_INFO_SIZE_NO_CODE: u64 = 41;

/// Byte size of an account-info diff for accounts with code (adds code_hash).
const ACCOUNT_INFO_SIZE_WITH_CODE: u64 = 73;

/// Fixed per-account overhead in the diff encoding.
const ACCOUNT_DIFF_OVERHEAD: u64 = 12;

/// Fixed byte cost for a newly created account (address + metadata).
const NEW_ACCOUNT_SIZE: u64 = 32;

// ── Per-account tracking ────────────────────────────────────────────

/// Per-account change tracking used by [`calc_diff_size`].
struct AccountChanges {
    info_changed: bool,
    storage_keys: BTreeSet<revm::primitives::StorageKey>,
}

impl Default for AccountChanges {
    fn default() -> Self {
        Self {
            info_changed: false,
            storage_keys: BTreeSet::new(),
        }
    }
}

// ── calc_diff_size ──────────────────────────────────────────────────

/// Calculates the uncompressed diff size from journal entries.
///
/// Walks every journal entry to determine which accounts had info changes
/// (nonce, balance, code) and which storage keys were modified. The caller
/// address is always marked as info-changed (nonce increment).
///
/// The final size is a weighted sum:
///   - account_diff × 32% + storage_diff × 66% + new_account_diff
pub(crate) fn calc_diff_size(
    journal: &[JournalEntry],
    state: &revm::state::EvmState,
    caller: Address,
) -> u64 {
    let mut changes: BTreeMap<Address, AccountChanges> = BTreeMap::new();
    let mut new_accounts: BTreeSet<Address> = BTreeSet::new();

    // Caller always has info changed (nonce increment).
    changes.entry(caller).or_default().info_changed = true;

    for entry in journal {
        match entry {
            JournalEntry::NonceChange { address, .. } | JournalEntry::NonceBump { address } => {
                changes.entry(*address).or_default().info_changed = true;
            }
            JournalEntry::BalanceTransfer { from, to, .. } => {
                changes.entry(*from).or_default().info_changed = true;
                changes.entry(*to).or_default().info_changed = true;
            }
            JournalEntry::BalanceChange { address, .. } => {
                changes.entry(*address).or_default().info_changed = true;
            }
            JournalEntry::StorageChanged { address, key, .. } => {
                changes
                    .entry(*address)
                    .or_default()
                    .storage_keys
                    .insert(*key);
            }
            JournalEntry::CodeChange { address } => {
                changes.entry(*address).or_default().info_changed = true;
            }
            JournalEntry::AccountCreated { address, .. } => {
                changes.entry(*address).or_default().info_changed = true;
                new_accounts.insert(*address);
            }
            JournalEntry::AccountDestroyed {
                address,
                target,
                had_balance,
                ..
            } => {
                changes.entry(*address).or_default().info_changed = true;
                if !had_balance.is_zero() {
                    changes.entry(*target).or_default().info_changed = true;
                }
            }
            // Warm/touch/transient entries do not affect state diff.
            JournalEntry::AccountWarmed { .. }
            | JournalEntry::AccountTouched { .. }
            | JournalEntry::StorageWarmed { .. }
            | JournalEntry::TransientStorageChange { .. } => {}
        }
    }

    // Calculate sizes.
    let mut account_diff: u64 = 0;
    let mut storage_diff: u64 = 0;

    for (addr, change) in &changes {
        if change.info_changed {
            let has_code = state
                .get(addr)
                .map(|acc| acc.info.code_hash != KECCAK_EMPTY)
                .unwrap_or(false);

            let db_size = if has_code {
                ACCOUNT_INFO_SIZE_WITH_CODE
            } else {
                ACCOUNT_INFO_SIZE_NO_CODE
            };
            account_diff += db_size + ACCOUNT_DIFF_OVERHEAD;
        }

        storage_diff += STORAGE_ENTRY_SIZE * change.storage_keys.len() as u64;
    }

    let new_account_diff = NEW_ACCOUNT_SIZE * new_accounts.len() as u64;

    // Weighted sum: account_diff * 32% + storage_diff * 66% + new_account_diff.
    (account_diff * ACCOUNT_DIFF_WEIGHT / 100)
        + (storage_diff * STORAGE_DIFF_WEIGHT / 100)
        + new_account_diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_journal_has_caller_diff() {
        let caller = Address::ZERO;
        let state = revm::state::EvmState::default();
        let journal = vec![];
        let size = calc_diff_size(&journal, &state, caller);
        // Caller always counted: (41 + 12) * 32 / 100 = 16
        assert_eq!(
            size,
            (ACCOUNT_INFO_SIZE_NO_CODE + ACCOUNT_DIFF_OVERHEAD) * ACCOUNT_DIFF_WEIGHT / 100
        );
    }

    #[test]
    fn test_storage_change_counted() {
        let caller = Address::ZERO;
        let addr = Address::repeat_byte(1);
        let state = revm::state::EvmState::default();
        let journal = vec![JournalEntry::StorageChanged {
            address: addr,
            key: Default::default(),
            had_value: Default::default(),
        }];
        let size = calc_diff_size(&journal, &state, caller);
        // caller account_diff + 1 storage entry
        let expected_account =
            (ACCOUNT_INFO_SIZE_NO_CODE + ACCOUNT_DIFF_OVERHEAD) * ACCOUNT_DIFF_WEIGHT / 100;
        let expected_storage = STORAGE_ENTRY_SIZE * STORAGE_DIFF_WEIGHT / 100;
        assert_eq!(size, expected_account + expected_storage);
    }
}
