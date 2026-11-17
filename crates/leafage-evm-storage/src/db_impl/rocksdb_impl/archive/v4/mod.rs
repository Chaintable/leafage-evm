//! Archive schema v4: account and storage versions in immutable key-ordered
//! page files outside RocksDB. See `docs/ArchiveSchemaV4.md`.
//!
//! A final block's state diff never changes once written, so history needs
//! neither in-place updates nor version garbage collection. v4 keeps it as
//!
//! - a **base**: page files sorted by key then block, holding every version
//!   up to some block, with a small in-memory index (one read per lookup,
//!   no filter needed);
//! - a **delta**: the blocks after the base, as a memtable and immutable
//!   segments of the same page format, each with an in-memory filter;
//! - a background rewrite that folds the delta into the base one file at a
//!   time.
//!
//! Block infos and code are in append-only files beside them; a v4 archive
//! has no RocksDB. The blocks still only in the memtable are in no file:
//! after a restart the archive's head is the last flushed block, and the
//! node fetches the later ones again.

mod codec;
mod file;
mod index;
mod mem;
mod side;
mod store;

pub(crate) mod convert;

#[cfg(test)]
mod tests;

pub(crate) use codec::Val;
pub(crate) use side::BlockRec;
pub(crate) use store::{BlockVersions, Options, Read, Store};

use crate::db_impl::error::Error;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Directory of the v4 store inside the archive database directory.
pub(crate) const STORE_DIR: &str = "v4";

fn corrupt(msg: impl std::fmt::Display) -> Error {
    Error::ArchiveV4(msg.to_string())
}

/// Device reads made for lookups (not for merges or rewrites), so that
/// background work can tell when the device is busy serving them.
pub(crate) struct IoStats {
    reads: AtomicU64,
    bytes: AtomicU64,
    nanos: AtomicU64,
}

impl IoStats {
    fn note_read(&self, bytes: u64, took: Duration) {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.nanos
            .fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
        let m = &crate::metrics::ARCHIVE_V4_METRICS;
        m.page_reads.increment(1);
        m.page_read_bytes.increment(bytes);
    }

    /// Reads, bytes and nanoseconds so far.
    pub(crate) fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.reads.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
            self.nanos.load(Ordering::Relaxed),
        )
    }
}

pub(crate) fn io_stats() -> &'static IoStats {
    static STATS: IoStats = IoStats {
        reads: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        nanos: AtomicU64::new(0),
    };
    &STATS
}
