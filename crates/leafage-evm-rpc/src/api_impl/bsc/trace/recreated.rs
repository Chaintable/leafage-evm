//! Storage of contracts that were destroyed and created again.
//!
//! The archive does not clear a contract's storage when it is destroyed, so
//! once the address is created again it still returns the slots the previous
//! incarnation left, which are zero on chain. A slot is left over when the
//! account at the block that wrote it is not the current one: it did not
//! exist, its code changed or its nonce went down, none of which happens to a
//! live contract (the system contracts, upgraded in place, aside). Only slots
//! written before Cancun qualify: since EIP-6780 a contract only goes away in
//! the transaction that created it, leaving no storage behind.
//!
//! An incarnation spans a range of blocks, so once a slot written at `w` is
//! known to be the current incarnation's, so is any written later, and once
//! it is known to be a previous one's, so is any written earlier: each
//! contract usually needs a single account read.

use super::replay::UPGRADABLE_CONTRACTS;
use alloy::primitives::{Address, B256, U256};
use leafage_evm_storage::{EvmStorageWrapper, StateDB};
use revm::bytecode::Bytecode;
use revm::primitives::KECCAK_EMPTY;
use revm::state::AccountInfo;
use revm::DatabaseRef;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// First BSC mainnet block with Cancun (timestamp 1718863500).
const CANCUN_BLOCK: u64 = 39_769_787;

/// What tells two incarnations of an account apart.
type Incarnation = Option<(B256, u64)>;

/// Read counts of a filter, for the per-block log line.
#[derive(Debug, Default)]
pub(crate) struct FilterStats {
    /// Storage slots read.
    pub slots: AtomicU64,
    /// Accounts read to check a slot's incarnation.
    pub accounts: AtomicU64,
    /// Slots read as zero because a previous incarnation wrote them.
    pub zeroed: AtomicU64,
}

/// What is known about a contract's slots so far.
#[derive(Debug, Clone, Copy)]
struct Known {
    current: Incarnation,
    /// Slots written at or after this block belong to `current`.
    current_from: u64,
    /// Slots written at or before this block belong to a previous one.
    previous_until: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct RecreatedStorageFilter<T: StateDB> {
    inner: EvmStorageWrapper<T>,
    /// The block the state is at.
    height: u64,
    stats: Arc<FilterStats>,
    known: Mutex<HashMap<Address, Known>>,
}

impl<T: StateDB> RecreatedStorageFilter<T> {
    pub(crate) fn new(inner: EvmStorageWrapper<T>, height: u64, stats: Arc<FilterStats>) -> Self {
        Self {
            inner,
            height,
            stats,
            known: Mutex::new(HashMap::new()),
        }
    }

    fn incarnation(account: Option<AccountInfo>) -> Incarnation {
        account
            .filter(|info| !is_empty(info))
            .map(|info| (normalize_code_hash(info.code_hash), info.nonce))
    }

    /// Whether a slot of `address` written at `written` belongs to the
    /// current incarnation; `None` if the backend keeps no history.
    fn is_current(&self, address: Address, written: u64) -> Result<Option<bool>, T::Error> {
        let known = self.known.lock().unwrap().get(&address).copied();
        let mut known = match known {
            Some(known) => {
                if written >= known.current_from {
                    return Ok(Some(true));
                }
                if known.previous_until.is_some_and(|until| written <= until) {
                    return Ok(Some(false));
                }
                known
            }
            None => {
                self.stats.accounts.fetch_add(1, Ordering::Relaxed);
                Known {
                    current: Self::incarnation(self.inner.basic_ref(address)?),
                    current_from: self.height,
                    previous_until: None,
                }
            }
        };
        self.stats.accounts.fetch_add(1, Ordering::Relaxed);
        let Some(then) = self.inner.basic_at_ref(address, written)? else {
            return Ok(None);
        };
        let same = match (Self::incarnation(then), known.current) {
            (Some((code_then, nonce_then)), Some((code_now, nonce_now))) => {
                code_then == code_now && nonce_now >= nonce_then
            }
            _ => false,
        };
        if same {
            known.current_from = known.current_from.min(written);
        } else {
            known.previous_until = Some(known.previous_until.map_or(written, |u| u.max(written)));
        }
        self.known.lock().unwrap().insert(address, known);
        Ok(Some(same))
    }
}

impl<T: StateDB + Debug> DatabaseRef for RecreatedStorageFilter<T> {
    type Error = T::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.inner.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner.code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.stats.slots.fetch_add(1, Ordering::Relaxed);
        let (value, written) = self.inner.storage_and_height_ref(address, index)?;
        let Some(written) = written else {
            return Ok(value);
        };
        if value.is_zero()
            || written >= CANCUN_BLOCK
            || written >= self.height
            || UPGRADABLE_CONTRACTS.contains(&address)
        {
            return Ok(value);
        }
        if self.is_current(address, written)? != Some(false) {
            return Ok(value);
        }
        self.stats.zeroed.fetch_add(1, Ordering::Relaxed);
        Ok(U256::ZERO)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash_ref(number)
    }
}

fn is_empty(info: &AccountInfo) -> bool {
    info.balance.is_zero() && info.nonce == 0 && normalize_code_hash(info.code_hash) == KECCAK_EMPTY
}

fn normalize_code_hash(code_hash: B256) -> B256 {
    if code_hash.is_zero() {
        KECCAK_EMPTY
    } else {
        code_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, keccak256};
    use revm::database_interface::DBErrorMarker;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    #[derive(Debug, thiserror::Error)]
    #[error("mock")]
    struct MockErr;
    impl DBErrorMarker for MockErr {}

    /// One contract, `old` before `recreated_at` and `new` from there on;
    /// every slot holds 7 and was written at the next of `written`.
    #[derive(Debug)]
    struct History {
        recreated_at: u64,
        old: Option<AccountInfo>,
        new: Option<AccountInfo>,
        written: RefCell<VecDeque<u64>>,
        lookups: Cell<u32>,
    }

    impl StateDB for History {
        type Error = MockErr;

        fn basic(&self, _: B256) -> Result<Option<AccountInfo>, MockErr> {
            Ok(self.new.clone())
        }
        fn code_by_hash(&self, _: B256) -> Result<Bytecode, MockErr> {
            Ok(Bytecode::default())
        }
        fn storage(&self, _: B256, _: B256) -> Result<U256, MockErr> {
            Ok(U256::from(7))
        }
        fn block_hash(&self, _: u64) -> Result<B256, MockErr> {
            Ok(B256::ZERO)
        }
        fn storage_and_height(&self, _: B256, _: B256) -> Result<(U256, Option<u64>), MockErr> {
            Ok((U256::from(7), self.written.borrow_mut().pop_front()))
        }
        fn basic_at(&self, _: B256, block: u64) -> Result<Option<Option<AccountInfo>>, MockErr> {
            self.lookups.set(self.lookups.get() + 1);
            Ok(Some(if block < self.recreated_at {
                self.old.clone()
            } else {
                self.new.clone()
            }))
        }
    }

    fn contract(code: u8, nonce: u64) -> Option<AccountInfo> {
        Some(AccountInfo {
            nonce,
            code_hash: keccak256([code]),
            ..Default::default()
        })
    }

    /// Reads one slot per entry of `written` from a state at `height`.
    fn read(history: History, height: u64) -> (Vec<u64>, u32) {
        let reads = history.written.borrow().len();
        let filter = RecreatedStorageFilter::new(
            EvmStorageWrapper {
                db: history,
                ovm_address: None,
                normalize_state_key: false,
            },
            height,
            Arc::default(),
        );
        let address = address!("00000000000000000000000000000000000000aa");
        let values = (0..reads)
            .map(|_| filter.storage_ref(address, U256::ZERO).unwrap().to::<u64>())
            .collect();
        (values, filter.inner.db.lookups.get())
    }

    fn history(old: Option<AccountInfo>, new: Option<AccountInfo>, written: &[u64]) -> History {
        History {
            recreated_at: 1_500,
            old,
            new,
            written: RefCell::new(written.iter().copied().collect()),
            lookups: Cell::new(0),
        }
    }

    #[test]
    fn slots_of_a_previous_incarnation_read_zero() {
        let one = |old, new| read(history(old, new, &[1_000]), u64::MAX).0[0];
        // Same contract: nonce may grow.
        assert_eq!(one(contract(1, 1), contract(1, 3)), 7);
        // Re-deployed with other code, or the nonce went back.
        assert_eq!(one(contract(1, 1), contract(2, 1)), 0);
        assert_eq!(one(contract(1, 5), contract(1, 1)), 0);
        // Gone, or not there when the slot was written.
        assert_eq!(one(contract(1, 1), None), 0);
        assert_eq!(one(None, contract(1, 1)), 0);
        // Written after Cancun: nothing could have destroyed it since.
        let after_cancun = history(contract(1, 1), contract(2, 1), &[CANCUN_BLOCK]);
        assert_eq!(read(after_cancun, u64::MAX).0, [7]);
    }

    #[test]
    fn one_lookup_per_side_of_the_recreation() {
        // Re-created at 1500; the state is at 2000.
        let slots = history(
            contract(1, 1),
            contract(2, 1),
            &[1_600, 1_700, 1_000, 900, 2_000],
        );
        let (values, lookups) = read(slots, 2_000);
        assert_eq!(values, [7, 7, 0, 0, 7]);
        // 1700 is after a slot known current, 900 before one known previous
        // and 2000 is the state's own block.
        assert_eq!(lookups, 2);
    }
}
