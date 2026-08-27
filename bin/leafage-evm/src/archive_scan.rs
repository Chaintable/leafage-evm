//! `leafage-evm archive-scan` — read-only forensic dumper for an archive
//! RocksDB.
//!
//! Built against the same `rocksdb`/`librocksdb-sys` the node links, so it
//! always matches the on-disk format without needing an external `ldb` (whose
//! distro build is usually too old for the node's RocksDB).
//!
//! Scans one column family under a key prefix and prints, for each record, the
//! full key, the trailing 8-byte height tail, both legacy/inverted decodings of
//! that height, and the value. This makes an orphaned `0xFF..FF` tail (the
//! pre-#104 dual-write `u64::MAX` latest pointer) immediately visible next to
//! real per-block versions.
//!
//! The key prefix is derived for you from `--address` (and optional `--slot`):
//!   * account CF: prefix = `keccak256(address)`
//!   * storage CF: prefix = `keccak256(address)` (all slots) or
//!     `keccak256(address) ‖ keccak256(slot)` when `--slot` is given.
//! Use `--prefix-hex` to pass a raw prefix instead (advanced; e.g. for
//! normalized state keys).
//!
//! Examples:
//!   leafage-evm archive-scan --db-path /base-rpc --cf account \
//!     --address 0x4D0D2570Ea2962D0dc586efBAa4F8432029Fa42C
//!   leafage-evm archive-scan --db-path /base-rpc --cf storage \
//!     --address 0x833589fcd6edb6e08f4c7c32d4f71b54bda02913 --slot 0x0 --limit 50

use alloy::primitives::keccak256;
use anyhow::{anyhow, Result};
use clap::Parser;
use rocksdb::{Direction, IteratorMode, Options, ReadOptions, DB};
use std::path::PathBuf;
use tracing::info;

/// `leafage-evm archive-scan` command
#[derive(Debug, Parser)]
pub struct Command {
    /// Path to the archive RocksDB
    #[arg(long, value_name = "PATH")]
    db_path: PathBuf,

    /// Column family to scan. Readable name or raw number (1-6):
    /// account(4), storage(5), code(6), block-info(2), block-hash(3),
    /// latest-block-hash(1).
    #[arg(long, default_value = "account")]
    cf: String,

    /// Account address (20-byte hex). Its keccak256 hash is the key prefix.
    #[arg(long, value_name = "0x..", conflicts_with = "prefix_hex")]
    address: Option<String>,

    /// Storage slot (hex, up to 32 bytes), only meaningful with `--cf storage`.
    /// Appended to the hashed address as keccak256(slot). Assumes
    /// non-normalized state keys; use `--prefix-hex` for normalized chains.
    #[arg(long, value_name = "0x..", requires = "address")]
    slot: Option<String>,

    /// Raw key prefix in hex (advanced; bypasses --address/--slot hashing).
    #[arg(long, value_name = "HEX")]
    prefix_hex: Option<String>,

    /// Max number of records to print.
    #[arg(long, default_value_t = usize::MAX)]
    limit: usize,

    /// RocksDB max open files. Archive DBs have many SST files; the default
    /// caps open fds (use a table cache) to avoid "Too many open files".
    /// Set -1 for unlimited.
    #[arg(long, default_value_t = 256)]
    max_open_files: i32,
}

impl Command {
    pub async fn run(&mut self) -> Result<()> {
        let cf_num = resolve_cf(&self.cf)?;
        let prefix = self.resolve_prefix()?;

        info!(
            target: "archive_scan",
            "scanning cf={} ({}) db_path={:?} prefix=0x{}",
            self.cf, cf_num, self.db_path, alloy::hex::encode(&prefix),
        );

        // Read-only open. RocksDB requires every existing CF to be listed; the
        // archive backend uses the numeric names 1..=6.
        let mut opts = Options::default();
        opts.set_max_open_files(self.max_open_files);
        let cfs = ["1", "2", "3", "4", "5", "6"];
        let db = DB::open_cf_for_read_only(&opts, &self.db_path, cfs, false).map_err(|e| {
            anyhow!("open read-only failed (is the node stopped / DB lock free?): {e}")
        })?;
        let cf = db
            .cf_handle(cf_num)
            .ok_or_else(|| anyhow!("column family {cf_num} not found"))?;

        // total_order_seek so the scan is not silently truncated by the prefix
        // bloom filter.
        let mut ro = ReadOptions::default();
        ro.set_total_order_seek(true);

        let iter = db.iterator_cf_opt(cf, ro, IteratorMode::From(&prefix, Direction::Forward));

        // Only account(4)/storage(5) keys carry a versioned height tail.
        let versioned = matches!(cf_num, "4" | "5");
        let mut count = 0usize;
        for item in iter {
            let (key, value) = item?;
            if !key.starts_with(&prefix) {
                break; // left the prefix range
            }
            if count >= self.limit {
                break;
            }
            if versioned && key.len() >= 8 {
                // The u64 height lives in the last 8 bytes (big-endian).
                let tail = &key[key.len() - 8..];
                let raw = u64::from_be_bytes(tail.try_into().unwrap());
                println!(
                    "key=0x{} tail8=0x{} height[legacy]={} height[inverted]={} value=0x{}",
                    alloy::hex::encode(&key),
                    alloy::hex::encode(tail),
                    raw,
                    u64::MAX - raw,
                    alloy::hex::encode(&value),
                );
            } else {
                println!(
                    "key=0x{} value=0x{}",
                    alloy::hex::encode(&key),
                    alloy::hex::encode(&value),
                );
            }
            count += 1;
        }

        info!(
            target: "archive_scan",
            "scanned {count} record(s) under prefix 0x{}",
            alloy::hex::encode(&prefix),
        );
        Ok(())
    }

    /// Build the scan prefix from `--prefix-hex`, or from `--address`
    /// (+ optional `--slot`) by hashing as the archive backend does.
    fn resolve_prefix(&self) -> Result<Vec<u8>> {
        if let Some(hex) = &self.prefix_hex {
            return decode_hex(hex);
        }
        let address = self
            .address
            .as_ref()
            .ok_or_else(|| anyhow!("provide --address (or --prefix-hex)"))?;
        let addr_bytes = decode_hex(address)?;
        if addr_bytes.len() != 20 {
            return Err(anyhow!(
                "--address must be 20 bytes, got {} bytes",
                addr_bytes.len()
            ));
        }
        // Account/storage keys are prefixed by keccak256(address).
        let mut prefix = keccak256(&addr_bytes).to_vec();
        if let Some(slot) = &self.slot {
            let slot_bytes = decode_hex(slot)?;
            if slot_bytes.len() > 32 {
                return Err(anyhow!(
                    "--slot must be <= 32 bytes, got {} bytes",
                    slot_bytes.len()
                ));
            }
            // Left-pad the slot to 32 bytes, then keccak256 (= the stored index).
            let mut padded = [0u8; 32];
            padded[32 - slot_bytes.len()..].copy_from_slice(&slot_bytes);
            prefix.extend_from_slice(keccak256(padded).as_slice());
        }
        Ok(prefix)
    }
}

/// Decode a `0x`-prefixed (or bare) hex string.
fn decode_hex(s: &str) -> Result<Vec<u8>> {
    alloy::hex::decode(s.trim_start_matches("0x")).map_err(|e| anyhow!("invalid hex {s:?}: {e}"))
}

/// Map a readable column-family name (or raw number) to its on-disk CF name.
fn resolve_cf(name: &str) -> Result<&'static str> {
    let n = name.trim().to_ascii_lowercase().replace(['-', '_'], "");
    Ok(match n.as_str() {
        "1" | "latestblockhash" => "1",
        "2" | "blockhashtoblockinfo" | "blockinfo" => "2",
        "3" | "blocknumtoblockhash" | "blockhash" => "3",
        "4" | "addresstoaccount" | "account" | "accounts" => "4",
        "5" | "addresstostorage" | "storage" | "storages" => "5",
        "6" | "hashtocode" | "code" | "codes" => "6",
        _ => {
            return Err(anyhow!(
                "unknown column family {name:?}; use one of: \
                 account, storage, code, block-info, block-hash, latest-block-hash (or 1-6)"
            ))
        }
    })
}
