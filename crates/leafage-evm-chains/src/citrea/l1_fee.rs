use crate::citrea::CitreaContext;
use alloy_evm::Database;
use leafage_evm_types::Bytecode;
use revm::context::transaction::AuthorizationTr;
use revm::context::{ContextTr, Transaction};
use revm::context_interface::journaled_state::entry::SelfdestructionRevertStatus;
use revm::primitives::{address, Address, KECCAK_EMPTY, U256};
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
pub const SYSTEM_SIGNER: Address = address!("deaddeaddeaddeaddeaddeaddeaddeaddeaddead");
const STORAGE_DISCOUNTED_PERCENTAGE: usize = 66;
const ACCOUNT_DISCOUNTED_PERCENTAGE: usize = 32;

/// Calculates the diff of the modified state.
pub(crate) fn calc_diff_size<DB>(context: &mut CitreaContext<DB>) -> usize
where
    DB: Database,
{
    let (journaled_state, tx) = (&context.journaled_state, &context.tx);

    // For each call there is a journal entry.
    // We need to iterate over all journal entries to get the size of the diff.
    let journal = journaled_state.journal.iter();
    let state = &journaled_state.state;

    #[derive(Default)]
    struct AccountChange<'a> {
        storage_changes: BTreeSet<&'a U256>,
        account_info_changed: bool, // implies balance, nonce or code_hash changed
    }

    let mut account_changes: BTreeMap<&Address, AccountChange<'_>> = BTreeMap::new();

    // tx.from always has `account_info_changed` because its nonce is incremented
    let tx_caller = tx.caller();
    let from = account_changes.entry(&tx_caller).or_default();
    from.account_info_changed = true;

    // Special handling for eip7702 transactions
    // as there is no journal entry for changes on the authority

    // collecting then consuming the iterator
    // to avoid borrowing issues
    // also not doing tx type check as authorization_list will return empty list
    let auths = tx
        .authorization_list()
        .filter_map(|auth| {
            let delegated_to = auth.address();
            let authority = auth.authority();
            authority.map(|authority| (authority, delegated_to))
        })
        .collect::<Vec<_>>();

    for (authority, delegated_to) in &auths {
        // if returns None, the authorization failed at one of the following checks:
        // - if the chain id check failed
        // - if nonce was u64::MAX
        // - if the signer couldn't be recovered <-- this case is not possible as we checked this on the above
        //   if let
        if let Some(authority_in_state) = journaled_state.state.get(authority) {
            // if the final code of the authority is equal to delegated address
            // or the delegated address is zero and the account code hash is KECCAK_EMPTY
            // we know the authorization went through
            if (delegated_to == &Address::ZERO && authority_in_state.info.code_hash == KECCAK_EMPTY)
                || authority_in_state
                    .info
                    .code
                    .as_ref()
                    .is_some_and(|code| *code == Bytecode::new_eip7702(*delegated_to))
            {
                // we set account changed for the authority
                let account = account_changes.entry(authority).or_default();
                account.account_info_changed = true;
            }
        }
    }

    for entry in journal {
        match entry {
            JournalEntry::NonceChange { address, .. } => {
                let account = account_changes.entry(address).or_default();
                account.account_info_changed = true;
            }
            JournalEntry::BalanceTransfer { from, to, .. } => {
                // No need to check balance for 0 value sent, revm does not add it to the journal
                let from = account_changes.entry(from).or_default();
                from.account_info_changed = true;
                let to = account_changes.entry(to).or_default();
                to.account_info_changed = true;
            }
            JournalEntry::StorageChanged { address, key, .. } => {
                let account = account_changes.entry(address).or_default();
                account.storage_changes.insert(key);
            }
            JournalEntry::CodeChange { address } => {
                let account = account_changes.entry(address).or_default();
                account.account_info_changed = true;
            }
            // Only added to the journal on smart contract creation
            JournalEntry::AccountCreated { address, .. } => {
                let account = account_changes.entry(address).or_default();
                account.account_info_changed = true;
            }
            JournalEntry::AccountDestroyed {
                address,
                target,
                destroyed_status,
                had_balance,
            } => {
                // This event is produced only if acc.is_created() || !is_cancun_enabled
                // State is not changed:
                // * if we are after Cancun upgrade and
                // * Selfdestruct account that is created in the same transaction and
                // * Specify the target is same as selfdestructed account. The balance stays unchanged.

                if matches!(
                    destroyed_status,
                    SelfdestructionRevertStatus::RepeatedSelfdestruction
                ) {
                    // It was already destroyed before in the log, no need to do anything.
                    continue;
                }

                // transferred balance causes account diff change on target
                if address != target && !had_balance.is_zero() {
                    // mark changes to the target account
                    let target = account_changes.entry(target).or_default();
                    target.account_info_changed = true;
                }
            }
            _ => {}
        }
    }

    // Check if it's a new address to charge for new index
    let mut addresses_to_check = Vec::with_capacity(account_changes.len());

    let mut account_based_diff = 0usize;
    let mut storage_based_diff = 0usize;

    for (addr, account) in account_changes {
        // cloning addresses to avoid borrowing issues
        addresses_to_check.push(*addr);

        // Apply size of account_info
        if account.account_info_changed {
            let db_account_size = {
                let account = &state[addr];
                if account.info.code_hash == KECCAK_EMPTY {
                    DB_ACCOUNT_SIZE_EOA
                } else {
                    DB_ACCOUNT_SIZE_CONTRACT
                }
            };
            // Account size is added because when any of those changes the db account is written to the state
            // because these fields are part of the account info and not state values
            account_based_diff += db_account_size + DB_ACCOUNT_KEY_SIZE;
        }

        // Apply size of changed slots
        let slot_size = STORAGE_KEY_SIZE + STORAGE_VALUE_SIZE; // key + value;

        storage_based_diff += slot_size * account.storage_changes.len();

        // No checks on code change as it is not part of the state diff
    }
    let mut new_account_based_diff = 0usize;
    for addr in addresses_to_check {
        if context.db_mut().basic(addr).ok().flatten().is_none() {
            new_account_based_diff += ACCOUNT_IDX_KEY_SIZE + ACCOUNT_IDX_SIZE;
        }
    }

    // final diff size
    (account_based_diff * ACCOUNT_DISCOUNTED_PERCENTAGE / 100)
        + (storage_based_diff * STORAGE_DISCOUNTED_PERCENTAGE / 100)
        + new_account_based_diff
}
