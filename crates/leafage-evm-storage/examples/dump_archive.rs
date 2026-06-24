//! Read-only forensic dumper for an archive RocksDB, built against the same
//! `librocksdb-sys` (RocksDB 10.7.5) the node links — so it always matches the
//! on-disk format without needing an external `ldb`.
//!
//! Scans one column family for keys under a hex prefix and prints, for each
//! record, the full key, the trailing 8-byte height tail, the decoded height
//! (both legacy and inverted readings), and the value. This makes a `0xFF..FF`
//! tail (the inverted encoding of block 0 / genesis) immediately visible next
//! to legacy small-height tails in a mixed-encoding DB.
//!
//! Column families (see `StorageTypeColumn`):
//!   1=LatestBlockHash 2=BlockHashToBlockInfo 3=BlockNumToBlockHash
//!   4=AddressToAccount 5=AddressToStorage 6=HashToCode
//!
//! Usage:
//!   cargo run -p leafage-evm-storage --example dump_archive -- \
//!     <db_path> <cf> <prefix_hex> [limit]
//!
//! Example (the account whose newest record decoded to u64::MAX):
//!   cargo run -p leafage-evm-storage --example dump_archive -- \
//!     /base-rpc 4 a3d801dde614639348fc8c30d56642a5b287a4a10d9b1fff6591d4538e51bf06

use rocksdb::{Direction, IteratorMode, Options, ReadOptions, DB};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: {} <db_path> <cf> <prefix_hex> [limit]",
            args.first().map(String::as_str).unwrap_or("dump_archive")
        );
        std::process::exit(2);
    }
    let db_path = &args[1];
    let cf_name = &args[2];
    let prefix = alloy::hex::decode(args[3].trim_start_matches("0x"))
        .expect("prefix_hex must be valid hex");
    let limit: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);

    // Read-only open. RocksDB requires every existing CF to be listed; the
    // archive backend uses the numeric names below.
    let mut opts = Options::default();
    // Archive DBs have a very large number of SST files; the default
    // (max_open_files = -1) tries to keep them all open and hits the OS
    // file-descriptor limit ("Too many open files"). Bound it so RocksDB uses
    // a table cache instead. Override with ROCKSDB_MAX_OPEN_FILE if needed.
    let max_open = std::env::var("ROCKSDB_MAX_OPEN_FILE")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(256);
    opts.set_max_open_files(max_open);
    let cfs = ["1", "2", "3", "4", "5", "6"];
    let db = DB::open_cf_for_read_only(&opts, db_path, cfs, false)
        .expect("open_cf_for_read_only failed (is the node stopped / DB not locked?)");
    let cf = db
        .cf_handle(cf_name)
        .unwrap_or_else(|| panic!("column family {cf_name:?} not found"));

    // total_order_seek so the scan is not silently truncated by the prefix bloom.
    let mut ro = ReadOptions::default();
    ro.set_total_order_seek(true);

    let iter = db.iterator_cf_opt(
        cf,
        ro,
        IteratorMode::From(&prefix, Direction::Forward),
    );

    let mut count = 0usize;
    for item in iter {
        let (key, value) = item.expect("iterator error");
        if !key.starts_with(&prefix) {
            break; // left the prefix range
        }
        if count >= limit {
            break;
        }
        // Height tail is the last 32 bytes; the u64 lives in its last 8 bytes BE.
        let tail = &key[key.len().saturating_sub(8)..];
        let raw = u64::from_be_bytes(tail.try_into().unwrap());
        let legacy = raw;
        let inverted = u64::MAX - raw;
        println!(
            "key=0x{} tail8=0x{} height[legacy]={} height[inverted]={} value=0x{}",
            alloy::hex::encode(&key),
            alloy::hex::encode(tail),
            legacy,
            inverted,
            alloy::hex::encode(&value),
        );
        count += 1;
    }
    eprintln!("scanned {count} record(s) under prefix 0x{}", alloy::hex::encode(&prefix));
}
