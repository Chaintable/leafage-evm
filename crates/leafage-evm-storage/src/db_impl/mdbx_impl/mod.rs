pub mod archive;
pub mod snapshot;

pub use archive::DataBase as MDBXArchiveStorage;
pub use archive::MDBXOptions as MDBXArchiveOptions;
pub use archive::MDBXWriteBatch as MDBXArchiveWriteBatch;
pub use archive::StateDB as MDBXArchiveStateDB;

pub use snapshot::DataBase as MDBXStorage;
pub use snapshot::MDBXWriteBatch;
pub use snapshot::StateDB as MDBXStateDB;

// ===== Common Constants =====

/// 1 KB in bytes
pub const KILOBYTE: usize = 1024;
/// 1 MB in bytes
pub const MEGABYTE: usize = KILOBYTE * 1024;
/// 1 GB in bytes
pub const GIGABYTE: usize = MEGABYTE * 1024;
/// 1 TB in bytes
pub const TERABYTE: usize = GIGABYTE * 1024;

/// MDBX allows up to 32767 readers (`MDBX_READERS_LIMIT`), but we limit it to slightly below that
pub(crate) const DEFAULT_MAX_READERS: u64 = 32_000;

/// Returns the default page size that can be used in this OS.
pub(crate) fn default_page_size() -> usize {
    let os_page_size = page_size::get();
    // source: https://gitflic.ru/project/erthink/libmdbx/blob?file=mdbx.h#line-num-821
    let libmdbx_max_page_size = 0x10000;
    // May lead to errors if it's reduced further because of the potential size of the data.
    let min_page_size = 4096;
    os_page_size.clamp(min_page_size, libmdbx_max_page_size)
}
