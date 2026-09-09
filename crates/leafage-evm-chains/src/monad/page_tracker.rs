//! MIP-8 storage page tracking (`state3/page_tracker.hpp`,
//! `monad/db/storage_page.hpp`).
//!
//! From MONAD_TEN storage access is priced per *page*: a page holds the 128
//! slots that share `slot >> 7`. Inside a transaction a page is cold until it
//! is first read or written, the first value-changing write to it costs an
//! extra `page_write_cost`, and every time the number of non-zero slots the
//! transaction added to the page exceeds the previous peak, `page_growth_cost`
//! is charged.
//!
//! The tracker state must follow the journal: it is reset per transaction and
//! rolled back when a call frame reverts (Monad keeps it inside the per-account
//! state that `State::pop_reject` restores). revm's transient storage has
//! exactly that lifetime, so the per-page state is kept as transient storage
//! of a reserved pseudo account that no contract can execute. It never reaches
//! the state diff or tracers because transient storage is dropped at the end
//! of the transaction.

use revm::context_interface::JournalTr;
use revm::interpreter::Host;
use revm::primitives::{address, keccak256, Address, U256};

/// Pseudo account holding the page tracker state in transient storage.
/// `b"MonadPageTracker" ++ 00000001`.
pub(crate) const PAGE_TRACKER_ADDRESS: Address =
    address!("4d6f6e616450616765547261636b657200000001");

/// `storage_page_t::PAGE_KEY_SHIFT`: 128 slots per page.
pub(crate) const PAGE_KEY_SHIFT: usize = 7;

/// `compute_page_key`.
#[inline]
pub(crate) fn page_key(slot: U256) -> U256 {
    slot >> PAGE_KEY_SHIFT
}

/// `evmc_storage_status`, derived exactly like `AccountState::set_storage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StorageStatus {
    Assigned,
    Added,
    Deleted,
    Modified,
    DeletedAdded,
    ModifiedDeleted,
    DeletedRestored,
    AddedDeleted,
    ModifiedRestored,
}

/// `AccountState::set_storage` -> `zero_out_key` / `set_current_value`.
pub(crate) fn storage_status(original: U256, current: U256, value: U256) -> StorageStatus {
    if value.is_zero() {
        if current.is_zero() {
            StorageStatus::Assigned
        } else if original == current {
            StorageStatus::Deleted
        } else if original.is_zero() {
            StorageStatus::AddedDeleted
        } else {
            StorageStatus::ModifiedDeleted
        }
    } else if current.is_zero() {
        if original.is_zero() {
            StorageStatus::Added
        } else if value == original {
            StorageStatus::DeletedRestored
        } else {
            StorageStatus::DeletedAdded
        }
    } else if original == current && original != value {
        StorageStatus::Modified
    } else if original == value && original != current {
        StorageStatus::ModifiedRestored
    } else {
        StorageStatus::Assigned
    }
}

impl StorageStatus {
    /// `PageTracker::update_page` growth delta.
    const fn growth_delta(self) -> i16 {
        match self {
            Self::Added | Self::DeletedAdded | Self::DeletedRestored => 1,
            Self::Deleted | Self::ModifiedDeleted | Self::AddedDeleted => -1,
            Self::Assigned | Self::Modified | Self::ModifiedRestored => 0,
        }
    }
}

/// `PageTracker::PageState`, packed into one transient slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PageState {
    pub accessed: bool,
    pub dirty: bool,
    pub peak_growth: i16,
    pub current_growth: i16,
}

impl PageState {
    fn decode(value: U256) -> Self {
        let word = value.as_limbs()[0];
        Self {
            accessed: word & 1 != 0,
            dirty: word & 2 != 0,
            peak_growth: ((word >> 16) & 0xffff) as u16 as i16,
            current_growth: ((word >> 32) & 0xffff) as u16 as i16,
        }
    }

    fn encode(self) -> U256 {
        let word = u64::from(self.accessed)
            | (u64::from(self.dirty) << 1)
            | (u64::from(self.peak_growth as u16) << 16)
            | (u64::from(self.current_growth as u16) << 32);
        U256::from(word)
    }
}

/// Where the tracker keeps its state: transient storage reached either through
/// the interpreter [`Host`] or directly through the [`JournalTr`].
pub(crate) trait PageStore {
    fn load(&mut self, key: U256) -> U256;
    fn store(&mut self, key: U256, value: U256);
}

pub(crate) struct HostPageStore<'a, H: ?Sized>(pub &'a mut H);

impl<H: Host + ?Sized> PageStore for HostPageStore<'_, H> {
    fn load(&mut self, key: U256) -> U256 {
        self.0.tload(PAGE_TRACKER_ADDRESS, key)
    }

    fn store(&mut self, key: U256, value: U256) {
        self.0.tstore(PAGE_TRACKER_ADDRESS, key, value)
    }
}

pub(crate) struct JournalPageStore<'a, J: ?Sized>(pub &'a mut J);

impl<J: JournalTr + ?Sized> PageStore for JournalPageStore<'_, J> {
    fn load(&mut self, key: U256) -> U256 {
        self.0.tload(PAGE_TRACKER_ADDRESS, key)
    }

    fn store(&mut self, key: U256, value: U256) {
        self.0.tstore(PAGE_TRACKER_ADDRESS, key, value)
    }
}

fn tracker_key(address: Address, page: U256) -> U256 {
    let mut buf = [0u8; 52];
    buf[..20].copy_from_slice(address.as_slice());
    buf[20..].copy_from_slice(&page.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

/// `PageTracker::access_page`. Returns `true` when the page was cold.
pub(crate) fn access_page<S: PageStore>(store: &mut S, address: Address, slot: U256) -> bool {
    let key = tracker_key(address, page_key(slot));
    let mut state = PageState::decode(store.load(key));
    if state.accessed {
        return false;
    }
    state.accessed = true;
    store.store(key, state.encode());
    true
}

/// `PageTracker::update_page`. Returns `(first_page_write, grew_state)`.
pub(crate) fn update_page<S: PageStore>(
    store: &mut S,
    address: Address,
    slot: U256,
    status: StorageStatus,
) -> (bool, bool) {
    let key = tracker_key(address, page_key(slot));
    let mut state = PageState::decode(store.load(key));

    let value_changed = status != StorageStatus::Assigned;
    let mut first_page_write = false;
    if !state.dirty {
        first_page_write = value_changed;
        state.dirty = value_changed;
    }

    state.current_growth = state.current_growth.wrapping_add(status.growth_delta());
    let grew_state = state.current_growth > state.peak_growth;
    if grew_state {
        state.peak_growth = state.current_growth;
    }
    if value_changed {
        store.store(key, state.encode());
    }
    (first_page_write, grew_state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct MapStore(HashMap<U256, U256>);

    impl PageStore for MapStore {
        fn load(&mut self, key: U256) -> U256 {
            self.0.get(&key).copied().unwrap_or_default()
        }

        fn store(&mut self, key: U256, value: U256) {
            self.0.insert(key, value);
        }
    }

    const ADDR: Address = address!("00000000000000000000000000000000000000aa");

    #[test]
    fn page_key_groups_128_slots() {
        assert_eq!(page_key(U256::from(0)), U256::ZERO);
        assert_eq!(page_key(U256::from(127)), U256::ZERO);
        assert_eq!(page_key(U256::from(128)), U256::from(1));
        assert_eq!(page_key(U256::MAX), U256::MAX >> 7);
    }

    #[test]
    fn page_state_round_trips() {
        let s = PageState {
            accessed: true,
            dirty: false,
            peak_growth: 3,
            current_growth: -2,
        };
        assert_eq!(PageState::decode(s.encode()), s);
        assert_eq!(PageState::decode(U256::ZERO), PageState::default());
    }

    #[test]
    fn storage_status_matches_account_state_rules() {
        let z = U256::ZERO;
        let x = U256::from(1);
        let y = U256::from(2);
        assert_eq!(storage_status(z, z, z), StorageStatus::Assigned);
        assert_eq!(storage_status(z, z, x), StorageStatus::Added);
        assert_eq!(storage_status(x, x, z), StorageStatus::Deleted);
        assert_eq!(storage_status(x, x, y), StorageStatus::Modified);
        assert_eq!(storage_status(x, z, y), StorageStatus::DeletedAdded);
        assert_eq!(storage_status(x, y, z), StorageStatus::ModifiedDeleted);
        assert_eq!(storage_status(x, z, x), StorageStatus::DeletedRestored);
        assert_eq!(storage_status(z, x, z), StorageStatus::AddedDeleted);
        assert_eq!(storage_status(x, y, x), StorageStatus::ModifiedRestored);
        assert_eq!(storage_status(x, y, y), StorageStatus::Assigned);
        assert_eq!(storage_status(z, x, y), StorageStatus::Assigned);
        assert_eq!(storage_status(x, x, x), StorageStatus::Assigned);
    }

    #[test]
    fn access_is_cold_once_per_page() {
        let mut store = MapStore::default();
        assert!(access_page(&mut store, ADDR, U256::from(5)));
        assert!(!access_page(&mut store, ADDR, U256::from(5)));
        // same page, different slot
        assert!(!access_page(&mut store, ADDR, U256::from(100)));
        // next page
        assert!(access_page(&mut store, ADDR, U256::from(128)));
        // same slot, different account
        let other = address!("00000000000000000000000000000000000000bb");
        assert!(access_page(&mut store, other, U256::from(5)));
    }

    #[test]
    fn update_page_tracks_first_write_and_peak_growth() {
        let mut store = MapStore::default();
        // no-op write never dirties the page
        assert_eq!(
            update_page(&mut store, ADDR, U256::from(1), StorageStatus::Assigned),
            (false, false)
        );
        // first real write: page write + growth
        assert_eq!(
            update_page(&mut store, ADDR, U256::from(1), StorageStatus::Added),
            (true, true)
        );
        // second add in the same page: growth only
        assert_eq!(
            update_page(&mut store, ADDR, U256::from(2), StorageStatus::Added),
            (false, true)
        );
        // delete then re-add: back to peak, no growth
        assert_eq!(
            update_page(&mut store, ADDR, U256::from(2), StorageStatus::AddedDeleted),
            (false, false)
        );
        assert_eq!(
            update_page(&mut store, ADDR, U256::from(2), StorageStatus::Added),
            (false, false)
        );
        // third distinct add exceeds the peak
        assert_eq!(
            update_page(&mut store, ADDR, U256::from(3), StorageStatus::Added),
            (false, true)
        );
        // modify is a write without growth
        assert_eq!(
            update_page(&mut store, ADDR, U256::from(200), StorageStatus::Modified),
            (true, false)
        );
    }
}
