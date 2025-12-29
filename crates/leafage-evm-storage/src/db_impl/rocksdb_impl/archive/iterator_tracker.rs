//! Iterator tracker for Archive mode RocksDB StateDB instances.
//!
//! This module tracks active StateDB instances that hold RocksDB iterators
//! and automatically closes them after a configurable timeout to prevent
//! blocking SST file deletion during compaction.

use crate::metrics::STORAGE_METRICS;
use dashmap::DashMap;
use rocksdb::{DBRawIteratorWithThreadMode, DB};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, error, trace, warn};

/// Shared timeout flag that can be checked by StateDB instances
pub(crate) type TimeoutFlag = Arc<AtomicBool>;

/// Shared iterators that can be released by the tracker
pub(crate) struct SharedIterators {
    pub account_iterator: Mutex<Option<DBRawIteratorWithThreadMode<'static, DB>>>,
    pub storage_iterator: Mutex<Option<DBRawIteratorWithThreadMode<'static, DB>>>,
}

impl SharedIterators {
    pub fn new(
        account_iterator: DBRawIteratorWithThreadMode<'static, DB>,
        storage_iterator: DBRawIteratorWithThreadMode<'static, DB>,
    ) -> Arc<Self> {
        Arc::new(Self {
            account_iterator: Mutex::new(Some(account_iterator)),
            storage_iterator: Mutex::new(Some(storage_iterator)),
        })
    }

    /// Release both iterators
    pub fn release(&self) {
        let _ = self.account_iterator.lock().unwrap().take();
        let _ = self.storage_iterator.lock().unwrap().take();
    }
}

/// Default timeout for iterators (0 = disabled)
pub const DEFAULT_ITERATOR_TIMEOUT_SECS: u64 = 0;

/// Check interval for the background monitor thread (5 seconds)
const CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Counter for generating unique StateDB IDs
static STATEDB_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique ID for a StateDB instance
pub(super) fn next_statedb_id() -> u64 {
    STATEDB_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Information about an active StateDB instance
pub(super) struct StateDBInfo {
    /// Time when the StateDB was created
    created_at: Instant,

    /// Block number for logging
    block_num: u64,

    /// Shared timeout flag - can be checked by StateDB instances
    timed_out: TimeoutFlag,

    /// Shared iterators reference for forced release
    iterators: Arc<SharedIterators>,

    /// Whether iterators have been force-released
    force_released: bool,
}

impl std::fmt::Debug for StateDBInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateDBInfo")
            .field("created_at", &self.created_at)
            .field("block_num", &self.block_num)
            .field("timed_out", &self.timed_out.load(Ordering::Relaxed))
            .field("force_released", &self.force_released)
            .finish()
    }
}

/// Global tracker for StateDB instances with iterators
#[derive(Debug)]
pub(super) struct IteratorTracker {
    /// Timeout duration for iterators
    timeout: Duration,

    /// Map of active StateDB instances: id -> info
    active: DashMap<u64, StateDBInfo>,

    /// Counter for timed out but not yet dropped iterators
    timed_out_count: AtomicU64,

    /// Cumulative counter for force-released iterators
    force_released_count: AtomicU64,
}

impl IteratorTracker {
    /// Create a new tracker with the specified timeout
    pub(super) fn new(timeout_secs: u64) -> Arc<Self> {
        Arc::new(Self {
            timeout: Duration::from_secs(timeout_secs),
            active: DashMap::new(),
            timed_out_count: AtomicU64::new(0),
            force_released_count: AtomicU64::new(0),
        })
    }

    /// Register a new StateDB instance and return the shared timeout flag
    pub(super) fn register(
        &self,
        id: u64,
        block_num: u64,
        iterators: Arc<SharedIterators>,
    ) -> TimeoutFlag {
        let timed_out = Arc::new(AtomicBool::new(false));
        let info = StateDBInfo {
            created_at: Instant::now(),
            block_num,
            timed_out: timed_out.clone(),
            iterators,
            force_released: false,
        };

        self.active.insert(id, info);
        STORAGE_METRICS
            .active_iterators
            .set(self.active.len() as f64);

        trace!(
            target: "rocksdb_archive",
            id = id,
            block_num = block_num,
            "Registered StateDB iterator"
        );

        timed_out
    }

    /// Unregister a StateDB instance
    pub(super) fn unregister(&self, id: u64) -> bool {
        if let Some((_, info)) = self.active.remove(&id) {
            if info.timed_out.load(Ordering::Relaxed) {
                self.timed_out_count.fetch_sub(1, Ordering::Relaxed);
                STORAGE_METRICS
                    .timed_out_iterators
                    .set(self.timed_out_count.load(Ordering::Relaxed) as f64);
            }
            STORAGE_METRICS
                .active_iterators
                .set(self.active.len() as f64);

            trace!(
                target: "rocksdb_archive",
                id = id,
                "Unregistered StateDB iterator"
            );
            true
        } else {
            false
        }
    }

    /// Check if a StateDB has timed out
    #[allow(dead_code)]
    pub(super) fn is_timed_out(&self, id: u64) -> bool {
        self.active
            .get(&id)
            .map(|info| info.timed_out.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// Get the number of active iterators
    #[allow(dead_code)]
    pub(super) fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Check if the tracker is enabled (timeout > 0)
    pub(super) fn is_enabled(&self) -> bool {
        self.timeout.as_secs() > 0
    }

    /// Start the background monitor task using tokio (only if enabled)
    pub(super) fn start_monitor(self: Arc<Self>) {
        if !self.is_enabled() {
            return;
        }
        tokio::spawn(async move {
            self.monitor_loop().await;
        });
    }

    /// Main monitoring loop (async version using tokio)
    async fn monitor_loop(&self) {
        let double_timeout = self.timeout * 2;

        loop {
            tokio::time::sleep(CHECK_INTERVAL).await;

            let now = Instant::now();
            let mut newly_timed_out = Vec::new();
            let mut force_released = Vec::new();

            // Find and mark timed out iterators, force release at 2x timeout
            for mut entry in self.active.iter_mut() {
                let id = *entry.key();
                let info = entry.value_mut();
                let duration = now.duration_since(info.created_at);

                if !info.timed_out.load(Ordering::Relaxed) {
                    if duration > self.timeout {
                        info.timed_out.store(true, Ordering::Relaxed);
                        self.timed_out_count.fetch_add(1, Ordering::Relaxed);
                        newly_timed_out.push((id, info.block_num, duration));
                    }
                } else if !info.force_released && duration > double_timeout {
                    // Force release iterators at 2x timeout
                    info.iterators.release();
                    info.force_released = true;
                    force_released.push((id, info.block_num, duration));
                }
            }

            // Log warnings for newly timed out iterators
            for (id, block_num, duration) in newly_timed_out {
                warn!(
                    target: "rocksdb_archive",
                    id = id,
                    block_num = block_num,
                    duration_secs = duration.as_secs(),
                    "StateDB iterator timed out - holding RocksDB resources too long"
                );
            }

            // Unregister force-released iterators and log errors
            for (id, block_num, duration) in force_released {
                // Unregister from tracker (will update timed_out_count and active_iterators)
                self.unregister(id);

                // Update force released counter
                self.force_released_count.fetch_add(1, Ordering::Relaxed);

                error!(
                    target: "rocksdb_archive",
                    id = id,
                    block_num = block_num,
                    duration_secs = duration.as_secs(),
                    "Force released and unregistered StateDB iterators at 2x timeout"
                );
            }

            // Update metrics
            STORAGE_METRICS
                .timed_out_iterators
                .set(self.timed_out_count.load(Ordering::Relaxed) as f64);
            STORAGE_METRICS
                .force_released_iterators
                .set(self.force_released_count.load(Ordering::Relaxed) as f64);

            debug!(
                target: "rocksdb_archive",
                active = self.active.len(),
                timed_out = self.timed_out_count.load(Ordering::Relaxed),
                force_released = self.force_released_count.load(Ordering::Relaxed),
                "Iterator tracker status"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    /// Create empty SharedIterators for testing (no real RocksDB iterators)
    fn create_test_iterators() -> Arc<SharedIterators> {
        Arc::new(SharedIterators {
            account_iterator: Mutex::new(None),
            storage_iterator: Mutex::new(None),
        })
    }

    #[test]
    fn test_tracker_basic() {
        let tracker = IteratorTracker::new(60);

        let id = next_statedb_id();
        let iterators = create_test_iterators();
        let _timeout_flag = tracker.register(id, 12345, iterators);
        assert_eq!(tracker.active_count(), 1);
        assert!(!tracker.is_timed_out(id));

        tracker.unregister(id);
        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn test_tracker_timeout_detection() {
        let tracker = IteratorTracker::new(1); // 1 second timeout

        let id = next_statedb_id();
        let iterators = create_test_iterators();
        let timeout_flag = tracker.register(id, 12345, iterators);

        // Initially not timed out
        assert!(!tracker.is_timed_out(id));
        assert!(!timeout_flag.load(Ordering::Relaxed));

        // Wait for timeout + a bit
        sleep(Duration::from_millis(1100));

        // Manually check (simulating what monitor_loop does)
        let now = Instant::now();
        for entry in tracker.active.iter() {
            let info = entry.value();
            if now.duration_since(info.created_at) > tracker.timeout {
                info.timed_out.store(true, Ordering::Relaxed);
            }
        }

        assert!(tracker.is_timed_out(id));
        assert!(timeout_flag.load(Ordering::Relaxed));

        tracker.unregister(id);
        assert_eq!(tracker.active_count(), 0);
    }

    #[test]
    fn test_multiple_statedbs() {
        let tracker = IteratorTracker::new(60);

        let id1 = next_statedb_id();
        let id2 = next_statedb_id();
        let id3 = next_statedb_id();

        let _flag1 = tracker.register(id1, 100, create_test_iterators());
        let _flag2 = tracker.register(id2, 200, create_test_iterators());
        let _flag3 = tracker.register(id3, 300, create_test_iterators());

        assert_eq!(tracker.active_count(), 3);

        tracker.unregister(id2);
        assert_eq!(tracker.active_count(), 2);

        tracker.unregister(id1);
        tracker.unregister(id3);
        assert_eq!(tracker.active_count(), 0);
    }
}
