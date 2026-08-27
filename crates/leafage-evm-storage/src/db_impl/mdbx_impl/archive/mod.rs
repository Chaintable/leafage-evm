//! MDBX Archive database implementation for historical state storage.
//!
//! Data is stored in the following format:
//! ```text
//! +-------------------------+-------------------------+-------------------------+
//! |  LatestBlockHash        |  BlockHashToBlockInfo   |  BlockNumToBlockHash    |
//! +-------------------------+-------------------------+-------------------------+
//! |  AddressToAccount       |  AddressToStorage       |  HashToCode             |
//! +-------------------------+-------------------------+-------------------------+
//! ```
//!
//! Key formats:
//! - `LatestBlockHash`: 1 -> block_hash (single record)
//! - `BlockHashToBlockInfo`: block_hash(32) -> Block<H256> (JSON)
//! - `BlockNumToBlockHash`: block_num(32) -> block_hash(32)
//! - `AddressToAccount`: address(32) || block_num(32) -> SlimAccount (RLP)
//! - `AddressToStorage`: address(32) || key(32) || block_num(32) -> value(32)
//! - `HashToCode`: code_hash(32) -> code_bytes
//!
//! All block numbers use 32-byte big-endian encoding (U256) for compatibility with RocksDB.

use super::{default_page_size, DEFAULT_MAX_READERS, GIGABYTE, MEGABYTE, TERABYTE};
use crate::db::{BlockIterator, LatestStateDBIterator, StateDBProvider, StateDBRead, StateDBWrite};
use crate::db_impl::archive_encoding::{
    encode_account_key, encode_block_num, encode_slim_account, encode_storage_key,
    inverted_block_encoding,
};
use crate::db_impl::error::Error;
use crate::metrics::STORAGE_METRICS;
use alloy_rlp::Decodable;
use leafage_evm_types::{
    BlockId, BlockInfo, BlockNumberOrTag, Bytes, NewAccount, SlimAccount, H256, KECCAK256_EMPTY,
    U256,
};
use libmdbx::{
    Cursor, DatabaseFlags, Environment, EnvironmentFlags, Geometry, Mode, PageSize, SyncMode,
    Transaction, WriteFlags, RO, RW,
};
use serde_json::{from_slice, to_vec};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};
use std::path::Path;
use std::sync::Arc;
use tracing::{info, trace};

const LATEST_BLOCK_HASH_KEY: &[u8] = &[1u8];

// ===== Table Definition =====

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StorageTable {
    LatestBlockHash = 1,
    BlockHashToBlockInfo = 2,
    BlockNumToBlockHash = 3,
    AddressToAccount = 4,
    AddressToStorage = 5,
    HashToCode = 6,
}

impl StorageTable {
    fn to_str(&self) -> &'static str {
        match self {
            StorageTable::LatestBlockHash => "LatestBlockHash",
            StorageTable::BlockHashToBlockInfo => "BlockHashToBlockInfo",
            StorageTable::BlockNumToBlockHash => "BlockNumToBlockHash",
            StorageTable::AddressToAccount => "AddressToAccount",
            StorageTable::AddressToStorage => "AddressToStorage",
            StorageTable::HashToCode => "HashToCode",
        }
    }
}

impl Display for StorageTable {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_str())
    }
}

// ===== MDBX Configuration Options =====

/// MDBX database configuration options for performance tuning
#[derive(Debug, Clone)]
pub struct MDBXOptions {
    /// Initial database size (default: 256MB to reduce early growth)
    pub initial_size: usize,
    /// Maximum database size (default: 1TB)
    pub max_size: usize,
    /// Growth step when database needs to expand (default: 4GB)
    pub growth_step: usize,
    /// Sync mode for durability vs performance trade-off
    pub sync_mode: SyncMode,
}

impl Default for MDBXOptions {
    fn default() -> Self {
        Self {
            initial_size: 256 * MEGABYTE,
            max_size: 1 * TERABYTE,
            growth_step: 4 * GIGABYTE,
            sync_mode: SyncMode::NoMetaSync,
        }
    }
}

// ===== Data Structures =====

#[derive(Debug, Clone)]
pub struct DataBase {
    env: Environment,
    dbis: Arc<HashMap<&'static str, u32>>,
}

unsafe impl Send for DataBase {}
unsafe impl Sync for DataBase {}

pub struct StateDB {
    db: Arc<DataBase>,
    block_num: u64,
    /// `None` when the database is empty (no blocks committed yet).
    /// Stores full BlockInfo to preserve `other` fields (e.g. l1FeeRate).
    cached_block_info: Option<BlockInfo>,
}

impl Clone for StateDB {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            block_num: self.block_num,
            cached_block_info: self.cached_block_info.clone(),
        }
    }
}

impl Debug for StateDB {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateDB")
            .field("block_num", &self.block_num)
            .field("cached_block_info", &self.cached_block_info)
            .finish()
    }
}

unsafe impl Send for StateDB {}
unsafe impl Sync for StateDB {}

/// Write batch with deferred sorting for optimized sequential writes.
///
/// Performance optimizations:
/// - Caches Account and Storage writes, sorts them at commit time
/// - Sorted writes improve cursor locality and reduce B-tree traversal
/// - Other tables (block_hash, block_info, code) are written immediately
///
/// This design makes sorting transparent to callers - they can write data
/// in any order, and the batch will sort it before committing.
pub struct MDBXWriteBatch {
    txn: Transaction<RW>,
    /// Cached cursors for tables that use immediate writes.
    cursors: HashMap<&'static str, Cursor<RW>>,
    /// Cached account writes: (encoded_key, encoded_value)
    /// Value is None for account deletion (empty bytes will be written)
    account_cache: Vec<([u8; 64], Option<Vec<u8>>)>,
    /// Cached storage writes: (encoded_key, value_bytes)
    storage_cache: Vec<([u8; 96], [u8; 32])>,
}

impl MDBXWriteBatch {
    /// Append pre-encoded account writes to the deferred cache. Allows callers to
    /// run key/value encoding off the writer thread (e.g. in fetcher tasks during
    /// archive bulk ingest) and then hand the prepared entries to the batch.
    #[inline]
    pub fn extend_account_writes<I>(&mut self, items: I)
    where
        I: IntoIterator<Item = ([u8; 64], Option<Vec<u8>>)>,
    {
        self.account_cache.extend(items);
    }

    /// Append pre-encoded storage writes to the deferred cache. See
    /// [`extend_account_writes`](Self::extend_account_writes).
    #[inline]
    pub fn extend_storage_writes<I>(&mut self, items: I)
    where
        I: IntoIterator<Item = ([u8; 96], [u8; 32])>,
    {
        self.storage_cache.extend(items);
    }
}

// ===== DataBase Implementation =====

impl DataBase {
    pub fn open<P: AsRef<Path>>(path: P) -> Self {
        Self::open_with_options(path, MDBXOptions::default())
    }

    /// Open database with custom options for performance tuning
    pub fn open_with_options<P: AsRef<Path>>(path: P, options: MDBXOptions) -> Self {
        let path = path.as_ref();

        let mut inner_env = Environment::builder();

        inner_env.write_map();
        inner_env.set_max_dbs(256);

        let geometry = Geometry {
            size: Some(options.initial_size..(options.max_size)),
            growth_step: Some(options.growth_step as isize),
            shrink_threshold: Some(0),
            page_size: Some(PageSize::Set(default_page_size())),
        };

        inner_env.set_geometry(geometry);

        inner_env.set_flags(EnvironmentFlags {
            mode: Mode::ReadWrite {
                sync_mode: options.sync_mode,
            },
            no_rdahead: true,
            coalesce: true,
            no_meminit: true,
            liforeclaim: true,
            ..Default::default()
        });

        inner_env.set_max_readers(DEFAULT_MAX_READERS);
        inner_env.set_rp_augment_limit(512 * 1024);

        let mut dbis = HashMap::new();

        let env = inner_env
            .open(path)
            .expect("Failed to open MDBX environment");

        info!(
            target = "mdbx_archive",
            "Opened MDBX archive database at {:?}", path
        );

        // Create tables
        let txn = env.begin_rw_txn().expect("Failed to begin transaction");
        let t = txn
            .create_db(
                Some(StorageTable::LatestBlockHash.to_str()),
                DatabaseFlags::empty(),
            )
            .expect("Failed to create LatestBlockHash table");
        dbis.insert(StorageTable::LatestBlockHash.to_str(), t.dbi());

        let t = txn
            .create_db(
                Some(StorageTable::BlockHashToBlockInfo.to_str()),
                DatabaseFlags::empty(),
            )
            .expect("Failed to create BlockHashToBlockInfo table");
        dbis.insert(StorageTable::BlockHashToBlockInfo.to_str(), t.dbi());

        let t = txn
            .create_db(
                Some(StorageTable::BlockNumToBlockHash.to_str()),
                DatabaseFlags::empty(),
            )
            .expect("Failed to create BlockNumToBlockHash table");
        dbis.insert(StorageTable::BlockNumToBlockHash.to_str(), t.dbi());

        let t = txn
            .create_db(
                Some(StorageTable::AddressToAccount.to_str()),
                DatabaseFlags::empty(),
            )
            .expect("Failed to create AddressToAccount table");
        dbis.insert(StorageTable::AddressToAccount.to_str(), t.dbi());

        let t = txn
            .create_db(
                Some(StorageTable::AddressToStorage.to_str()),
                DatabaseFlags::empty(),
            )
            .expect("Failed to create AddressToStorage table");
        dbis.insert(StorageTable::AddressToStorage.to_str(), t.dbi());

        let t = txn
            .create_db(
                Some(StorageTable::HashToCode.to_str()),
                DatabaseFlags::empty(),
            )
            .expect("Failed to create HashToCode table");
        dbis.insert(StorageTable::HashToCode.to_str(), t.dbi());

        txn.commit().expect("Failed to commit transaction");

        Self {
            env,
            dbis: Arc::new(dbis),
        }
    }

    /// Manually sync/flush data to disk.
    /// This is important when using `UtterlyNoSync` mode to ensure data durability.
    /// Returns `true` if sync was needed, `false` if already synced.
    pub fn sync(&self, force: bool) -> Result<bool, Error> {
        self.env
            .sync(force)
            .map_err(|e| Error::UnSupported(format!("Failed to sync MDBX: {}", e)))
    }

    /// Read block hash by block number
    pub fn read_block_hash(&self, block_num: u64) -> Result<H256, Error> {
        let start = std::time::Instant::now();
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| Error::UnSupported(format!("Failed to begin read transaction: {}", e)))?;

        let dbi = self
            .dbis
            .get(StorageTable::BlockNumToBlockHash.to_str())
            .ok_or_else(|| Error::UnSupported("BlockNumToBlockHash table not found".to_string()))?;

        let block_num_bytes = encode_block_num(block_num);

        let block_hash_bytes: Option<Cow<'_, [u8]>> = txn
            .get(*dbi, &block_num_bytes)
            .map_err(|e| Error::UnSupported(format!("Failed to read block hash: {}", e)))?;

        STORAGE_METRICS
            .read_block_hash_latency
            .record(start.elapsed().as_secs_f64());

        match block_hash_bytes {
            Some(bytes) if !bytes.is_empty() => Ok(H256::from_slice(&bytes)),
            _ => Ok(H256::ZERO),
        }
    }

    /// Read block info by block hash
    pub fn read_block_info(&self, block_hash: H256) -> Result<Option<BlockInfo>, Error> {
        let start = std::time::Instant::now();
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| Error::UnSupported(format!("Failed to begin read transaction: {}", e)))?;

        let dbi = self
            .dbis
            .get(StorageTable::BlockHashToBlockInfo.to_str())
            .ok_or_else(|| {
                Error::UnSupported("BlockHashToBlockInfo table not found".to_string())
            })?;

        let block_hash_bytes: [u8; 32] = block_hash.into();
        let block_info_bytes: Option<Cow<'_, [u8]>> = txn
            .get(*dbi, &block_hash_bytes)
            .map_err(|e| Error::UnSupported(format!("Failed to read block info: {}", e)))?;

        STORAGE_METRICS
            .read_block_latency
            .record(start.elapsed().as_secs_f64());

        match block_info_bytes {
            Some(bytes) => {
                let block_info = from_slice::<BlockInfo>(&bytes)?;
                Ok(Some(block_info))
            }
            None => Ok(None),
        }
    }

    /// Read latest block hash
    pub fn read_latest_block_hash(&self) -> Result<H256, Error> {
        let start = std::time::Instant::now();
        let txn = self
            .env
            .begin_ro_txn()
            .map_err(|e| Error::UnSupported(format!("Failed to begin read transaction: {}", e)))?;

        let dbi = self
            .dbis
            .get(StorageTable::LatestBlockHash.to_str())
            .ok_or_else(|| Error::UnSupported("LatestBlockHash table not found".to_string()))?;

        let block_hash_bytes: Option<Cow<'_, [u8]>> = txn
            .get(*dbi, LATEST_BLOCK_HASH_KEY)
            .map_err(|e| Error::UnSupported(format!("Failed to read latest block hash: {}", e)))?;

        STORAGE_METRICS
            .read_latest_block_hash_latency
            .record(start.elapsed().as_secs_f64());

        match block_hash_bytes {
            Some(bytes) if !bytes.is_empty() => Ok(H256::from_slice(&bytes)),
            _ => Ok(H256::ZERO),
        }
    }

    /// Read latest block number
    pub fn read_latest_block_num(&self) -> Result<Option<u64>, Error> {
        let latest_hash = self.read_latest_block_hash()?;
        if latest_hash == H256::ZERO {
            return Ok(None);
        }
        let block_info = self.read_block_info(latest_hash)?;
        match block_info {
            Some(block) => Ok(Some(block.header.number)),
            None => Ok(None),
        }
    }
}

// ===== Helper Functions =====

/// Forward seek: smallest key `>= target_key` (MDBX `set_range`).
///
/// Returns `None` when no key `>= target_key` exists. With the descending
/// height encoding (`MAX - block_num`), the greatest version `<= H` is the
/// smallest key `>= address(‖slot)‖(MAX - H)`, so this lands on it directly.
///
/// # Important
///
/// When the queried slot has no version `<= H`, the landed key belongs to a
/// different (later) prefix. **Callers must verify the returned key matches the
/// expected prefix** before using the value; a mismatch means the slot was
/// first written after `H` (absent at that height). See the archive_encoding
/// module docs.
fn seek_ge<'a>(
    cursor: &'a mut Cursor<RO>,
    target_key: &[u8],
) -> Result<Option<(Cow<'a, [u8]>, Cow<'a, [u8]>)>, Error> {
    cursor
        .set_range::<Cow<'a, [u8]>, Cow<'a, [u8]>>(target_key)
        .map_err(|e| Error::UnSupported(format!("Set range failed: {}", e)))
}

/// Backward seek: largest key `<= target_key` (legacy ascending encoding).
///
/// With ascending height keys, the greatest version `<= H` is the largest key
/// `<= address(‖slot)‖H`. When no key `>= target` exists this returns the last
/// entry (a different prefix), so **callers must prefix-check** the result.
fn seek_le<'a>(
    cursor: &'a mut Cursor<RO>,
    target_key: &[u8],
) -> Result<Option<(Cow<'a, [u8]>, Cow<'a, [u8]>)>, Error> {
    match cursor.set_range::<Cow<'a, [u8]>, Cow<'a, [u8]>>(target_key) {
        Ok(Some((key, value))) => {
            if key.as_ref() == target_key {
                Ok(Some((key, value)))
            } else {
                // key > target_key, step back one.
                cursor
                    .prev::<Cow<'a, [u8]>, Cow<'a, [u8]>>()
                    .map_err(|e| Error::UnSupported(format!("Cursor prev failed: {}", e)))
            }
        }
        // All keys < target_key: the last entry is the largest <= target.
        Ok(None) => cursor
            .last::<Cow<'a, [u8]>, Cow<'a, [u8]>>()
            .map_err(|e| Error::UnSupported(format!("Cursor last failed: {}", e))),
        Err(e) => Err(Error::UnSupported(format!("Set range failed: {}", e))),
    }
}

/// Greatest version `<= block_num`, dispatching on the current encoding mode:
/// inverted -> forward [`seek_ge`]; legacy ascending -> backward [`seek_le`].
fn seek_version<'a>(
    cursor: &'a mut Cursor<RO>,
    target_key: &[u8],
) -> Result<Option<(Cow<'a, [u8]>, Cow<'a, [u8]>)>, Error> {
    if inverted_block_encoding() {
        seek_ge(cursor, target_key)
    } else {
        seek_le(cursor, target_key)
    }
}

// ===== Iterator Helpers =====

fn create_cursor(env: &Environment, table: StorageTable) -> Result<Cursor<RO>, Error> {
    let txn = env
        .begin_ro_txn()
        .map_err(|e| Error::UnSupported(format!("Failed to begin transaction: {}", e)))?;
    let db = txn
        .open_db(Some(table.to_str()))
        .map_err(|e| Error::UnSupported(format!("Failed to open db: {}", e)))?;
    txn.cursor(&db)
        .map_err(|e| Error::UnSupported(format!("Failed to create cursor: {}", e)))
}

fn decode_code(
    result: libmdbx::Result<(Cow<'_, [u8]>, Cow<'_, [u8]>)>,
) -> Result<(H256, Bytes), Error> {
    let (key, value) = result.map_err(|e| Error::UnSupported(format!("Iterator error: {}", e)))?;
    Ok((H256::from_slice(&key), Bytes::from(value.to_vec())))
}

fn decode_block_info(
    result: libmdbx::Result<(Cow<'_, [u8]>, Cow<'_, [u8]>)>,
) -> Result<BlockInfo, Error> {
    let (_, value) = result.map_err(|e| Error::UnSupported(format!("Iterator error: {}", e)))?;
    from_slice::<BlockInfo>(&value)
        .map_err(|e| Error::UnSupported(format!("Failed to decode block info: {}", e)))
}

fn decode_block_hash(
    result: libmdbx::Result<(Cow<'_, [u8]>, Cow<'_, [u8]>)>,
) -> Result<(u64, H256), Error> {
    let (key, value) = result.map_err(|e| Error::UnSupported(format!("Iterator error: {}", e)))?;
    let block_num: u64 = U256::from_be_slice(&key)
        .try_into()
        .map_err(|_| Error::UnSupported("Failed to convert block number".to_string()))?;
    Ok((block_num, H256::from_slice(&value)))
}

// ===== LatestStateDBIterator Implementation =====

impl LatestStateDBIterator for DataBase {
    /// Account address -> raw account
    /// Returns the latest state for each address (the record with highest block_num)
    fn account_iter(&self) -> impl Iterator<Item = Result<(H256, NewAccount), Error>> {
        match create_cursor(&self.env, StorageTable::AddressToAccount) {
            Ok(cursor) => {
                // Newest = FIRST record of each address prefix under inverted
                // encoding, LAST under legacy ascending encoding.
                let inverted = inverted_block_encoding();
                let mut iter = cursor.iter_slices().peekable();
                let mut consumed_prefix: Option<[u8; 32]> = None;

                Box::new(std::iter::from_fn(move || {
                    loop {
                        let item = iter.next()?;
                        if item.is_err() {
                            return Some(Err(Error::UnSupported(format!(
                                "Iterator error: {:?}",
                                item.unwrap_err()
                            ))));
                        }
                        let (key, value) = item.unwrap();
                        if key.len() < 64 {
                            continue;
                        }
                        let address_bytes: [u8; 32] = key[..32].try_into().unwrap();

                        let is_newest = if inverted {
                            let newest = consumed_prefix != Some(address_bytes);
                            consumed_prefix = Some(address_bytes);
                            newest
                        } else {
                            match iter.peek() {
                                Some(Ok((next_key, _))) => {
                                    next_key.len() < 32 || next_key[..32] != address_bytes
                                }
                                Some(Err(_)) => true,
                                None => true,
                            }
                        };
                        if !is_newest {
                            continue;
                        }

                        // Newest version is a deletion -> account absent at tip.
                        if value.is_empty() {
                            continue;
                        }

                        let address = H256::from_slice(&address_bytes);
                        let mut raw_account_slice: &[u8] = &value;
                        match SlimAccount::decode(&mut raw_account_slice) {
                            Ok(account) => {
                                return Some(Ok((
                                    address,
                                    NewAccount {
                                        address,
                                        balance: account.balance,
                                        nonce: account.nonce,
                                        code_hash: if account.code_hash.is_zero() {
                                            KECCAK256_EMPTY.0.into()
                                        } else {
                                            account.code_hash
                                        },
                                    },
                                )));
                            }
                            Err(e) => {
                                return Some(Err(Error::UnSupported(format!(
                                    "Failed to decode account: {}",
                                    e
                                ))));
                            }
                        }
                    }
                })) as Box<dyn Iterator<Item = _>>
            }
            Err(e) => Box::new(std::iter::once(Err(e))) as Box<dyn Iterator<Item = _>>,
        }
    }

    /// Code hash -> code
    fn code_iter(&self) -> impl Iterator<Item = Result<(H256, Bytes), Error>> {
        match create_cursor(&self.env, StorageTable::HashToCode) {
            Ok(cursor) => {
                Box::new(cursor.iter_slices().map(decode_code)) as Box<dyn Iterator<Item = _>>
            }
            Err(e) => Box::new(std::iter::once(Err(e))) as Box<dyn Iterator<Item = _>>,
        }
    }

    /// Account address | storage index -> storage value
    /// Returns the latest state for each (address, index) pair
    fn storage_iter(&self) -> impl Iterator<Item = Result<(H256, H256, U256), Error>> {
        match create_cursor(&self.env, StorageTable::AddressToStorage) {
            Ok(cursor) => {
                // Newest = FIRST record of each (address||index) prefix under
                // inverted encoding, LAST under legacy ascending encoding.
                let inverted = inverted_block_encoding();
                let mut iter = cursor.iter_slices().peekable();
                let mut consumed_prefix: Option<[u8; 64]> = None;

                Box::new(std::iter::from_fn(move || {
                    loop {
                        let item = iter.next()?;
                        if item.is_err() {
                            return Some(Err(Error::UnSupported(format!(
                                "Iterator error: {:?}",
                                item.unwrap_err()
                            ))));
                        }
                        let (key, value) = item.unwrap();
                        if key.len() < 96 {
                            continue;
                        }
                        let prefix: [u8; 64] = key[..64].try_into().unwrap();

                        let is_newest = if inverted {
                            let newest = consumed_prefix != Some(prefix);
                            consumed_prefix = Some(prefix);
                            newest
                        } else {
                            match iter.peek() {
                                Some(Ok((next_key, _))) => {
                                    next_key.len() < 64 || next_key[..64] != prefix
                                }
                                Some(Err(_)) => true,
                                None => true,
                            }
                        };
                        if !is_newest {
                            continue;
                        }

                        let storage_value = U256::from_be_slice(&value);
                        // Newest version is zero -> slot empty at tip.
                        if storage_value == U256::ZERO {
                            continue;
                        }

                        let address = H256::from_slice(&prefix[..32]);
                        let storage_key = H256::from_slice(&prefix[32..64]);
                        return Some(Ok((address, storage_key, storage_value)));
                    }
                })) as Box<dyn Iterator<Item = _>>
            }
            Err(e) => Box::new(std::iter::once(Err(e))) as Box<dyn Iterator<Item = _>>,
        }
    }
}

// ===== BlockIterator Implementation =====

impl BlockIterator for DataBase {
    fn block_info_iter(&self) -> impl Iterator<Item = Result<BlockInfo, Error>> {
        match create_cursor(&self.env, StorageTable::BlockHashToBlockInfo) {
            Ok(cursor) => {
                Box::new(cursor.iter_slices().map(decode_block_info)) as Box<dyn Iterator<Item = _>>
            }
            Err(e) => Box::new(std::iter::once(Err(e))) as Box<dyn Iterator<Item = _>>,
        }
    }

    fn block_hash_iter(&self) -> impl Iterator<Item = Result<(u64, H256), Error>> {
        match create_cursor(&self.env, StorageTable::BlockNumToBlockHash) {
            Ok(cursor) => {
                Box::new(cursor.iter_slices().map(decode_block_hash)) as Box<dyn Iterator<Item = _>>
            }
            Err(e) => Box::new(std::iter::once(Err(e))) as Box<dyn Iterator<Item = _>>,
        }
    }
}

// ===== StateDBProvider Implementation =====

impl StateDBProvider for Arc<DataBase> {
    type StateDBReadWrite = StateDB;

    fn db_at(&self, block_id: BlockId) -> Result<Option<Self::StateDBReadWrite>, Error> {
        let block_num: u64;
        let cached_block_info: Option<BlockInfo>;

        match block_id {
            BlockId::Hash(hash) => {
                let info = self.read_block_info(hash.block_hash)?;
                if info.is_none() {
                    return Ok(None);
                }
                let info = info.unwrap();
                block_num = info.header.number;
                cached_block_info = Some(info);
            }
            BlockId::Number(block_number_or_tag) => match block_number_or_tag {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                    // The latest hash's block info already carries the number,
                    // so resolve with one hash read + one info read instead of
                    // going num → hash → info again on top of
                    // read_latest_block_num (which reads both a first time).
                    // The latest pointer is the single source of truth here:
                    // BlockNumToBlockHash is written in the same transaction,
                    // so re-verifying it would only re-read what commit()
                    // already guarantees.
                    let latest_hash = self.read_latest_block_hash()?;
                    let info = if latest_hash == H256::ZERO {
                        None
                    } else {
                        self.read_block_info(latest_hash)?
                    };
                    match info {
                        Some(info) => {
                            block_num = info.header.number;
                            cached_block_info = Some(info);
                        }
                        None => {
                            block_num = 0;
                            cached_block_info = None;
                        }
                    }
                }
                BlockNumberOrTag::Number(num) => {
                    block_num = num;
                    let block_hash = self.read_block_hash(num)?;
                    if block_hash == H256::ZERO {
                        return Ok(None);
                    }
                    let info = self.read_block_info(block_hash)?;
                    if info.is_none() {
                        return Ok(None);
                    }
                    cached_block_info = Some(info.unwrap());
                }
                _ => return Err(Error::UnsupportedBlockId(block_id)),
            },
        }

        Ok(Some(StateDB {
            db: self.clone(),
            block_num,
            cached_block_info,
        }))
    }
}

// ===== StateDBRead Implementation =====

impl StateDBRead for StateDB {
    fn read_latest_block_hash(&self) -> Result<H256, Error> {
        match &self.cached_block_info {
            Some(info) => Ok(info.header.hash),
            None => self.db.read_latest_block_hash(),
        }
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<BlockInfo>, Error> {
        if let Some(info) = &self.cached_block_info {
            if block_hash == info.header.hash {
                return Ok(Some(info.clone()));
            }
        }
        self.db.read_block_info(block_hash)
    }

    fn read_block_hash(&self, block_num: u64) -> Result<H256, Error> {
        if let Some(info) = &self.cached_block_info {
            if block_num == self.block_num {
                return Ok(info.header.hash);
            }
        }
        self.db.read_block_hash(block_num)
    }

    fn read_account(&self, address: H256) -> Result<Option<NewAccount>, Error> {
        let start = std::time::Instant::now();

        let txn =
            self.db.env.begin_ro_txn().map_err(|e| {
                Error::UnSupported(format!("Failed to begin read transaction: {}", e))
            })?;

        let db = txn
            .open_db(Some(StorageTable::AddressToAccount.to_str()))
            .map_err(|e| Error::UnSupported(format!("Failed to open db: {}", e)))?;

        let mut cursor = txn
            .cursor(&db)
            .map_err(|e| Error::UnSupported(format!("Failed to create cursor: {}", e)))?;

        // Key format: address(32) || block_num(32)
        let target_key = encode_account_key(address, self.block_num);

        // Greatest version <= block_num (inverted -> forward, legacy -> backward).
        let result = match seek_version(&mut cursor, &target_key)? {
            Some((key, value)) => {
                // Verify the address prefix matches
                if key.len() < 32 || &key[..32] != address.as_slice() {
                    Ok(None)
                } else if value.is_empty() {
                    // Empty value means deleted account
                    Ok(None)
                } else {
                    let mut raw_account_slice: &[u8] = &value;
                    let account = SlimAccount::decode(&mut raw_account_slice).map_err(|e| {
                        Error::UnSupported(format!("Failed to decode account: {}", e))
                    })?;

                    Ok(Some(NewAccount {
                        address,
                        balance: account.balance,
                        nonce: account.nonce,
                        code_hash: if account.code_hash.is_zero() {
                            KECCAK256_EMPTY.0.into()
                        } else {
                            account.code_hash
                        },
                    }))
                }
            }
            None => Ok(None),
        };

        STORAGE_METRICS
            .read_account_latency
            .record(start.elapsed().as_secs_f64());

        result
    }

    fn read_storage(&self, address: H256, key: H256) -> Result<U256, Error> {
        let start = std::time::Instant::now();

        let txn =
            self.db.env.begin_ro_txn().map_err(|e| {
                Error::UnSupported(format!("Failed to begin read transaction: {}", e))
            })?;

        let db = txn
            .open_db(Some(StorageTable::AddressToStorage.to_str()))
            .map_err(|e| Error::UnSupported(format!("Failed to open db: {}", e)))?;

        let mut cursor = txn
            .cursor(&db)
            .map_err(|e| Error::UnSupported(format!("Failed to create cursor: {}", e)))?;

        // Key format: address(32) || storage_key(32) || block_num(32)
        let target_key = encode_storage_key(address, key, self.block_num);

        // Greatest version <= block_num (inverted -> forward, legacy -> backward).
        let result = match seek_version(&mut cursor, &target_key)? {
            Some((found_key, value)) => {
                // Verify the address and storage key prefix match
                if found_key.len() < 64
                    || &found_key[..32] != address.as_slice()
                    || &found_key[32..64] != key.as_slice()
                {
                    U256::ZERO
                } else if value.is_empty() {
                    // Empty value means deleted storage slot
                    U256::ZERO
                } else {
                    U256::from_be_slice(&value)
                }
            }
            None => U256::ZERO,
        };

        STORAGE_METRICS
            .read_storage_latency
            .record(start.elapsed().as_secs_f64());

        Ok(result)
    }

    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, Error> {
        let start = std::time::Instant::now();

        let txn =
            self.db.env.begin_ro_txn().map_err(|e| {
                Error::UnSupported(format!("Failed to begin read transaction: {}", e))
            })?;

        let dbi = self
            .db
            .dbis
            .get(StorageTable::HashToCode.to_str())
            .ok_or_else(|| Error::UnSupported("HashToCode table not found".to_string()))?;

        let code_hash_bytes: [u8; 32] = code_hash.into();
        let code: Option<Cow<'_, [u8]>> = txn
            .get(*dbi, &code_hash_bytes)
            .map_err(|e| Error::UnSupported(format!("Failed to read code: {}", e)))?;

        let result = match code {
            Some(bytes) => Ok(Some(Bytes::from(bytes.to_vec()))),
            None => Ok(None),
        };

        STORAGE_METRICS
            .read_code_latency
            .record(start.elapsed().as_secs_f64());

        result
    }
}

// ===== StateDBWrite Implementation =====

/// StateDB delegates all write operations to Arc<DataBase>
impl StateDBWrite for StateDB {
    type DBWriteBatch = MDBXWriteBatch;

    fn prepare_write_batch(&self) -> Result<Self::DBWriteBatch, Error> {
        self.db.prepare_write_batch()
    }

    fn write_latest_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_hash: H256,
    ) -> Result<(), Error> {
        self.db.write_latest_block_hash(batch, block_hash)
    }

    fn write_block_info(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_info: BlockInfo,
    ) -> Result<(), Error> {
        self.db.write_block_info(batch, block_info)
    }

    fn write_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_num: u64,
        block_hash: H256,
    ) -> Result<(), Error> {
        self.db.write_block_hash(batch, block_num, block_hash)
    }

    fn write_account(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        block_num: u64,
        raw_account: Option<NewAccount>,
    ) -> Result<(), Error> {
        self.db
            .write_account(batch, address, block_num, raw_account)
    }

    fn write_code(
        &self,
        batch: &mut Self::DBWriteBatch,
        code_hash: H256,
        code: Bytes,
    ) -> Result<(), Error> {
        self.db.write_code(batch, code_hash, code)
    }

    fn write_storage(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        key: H256,
        block_num: u64,
        value: U256,
    ) -> Result<(), Error> {
        self.db.write_storage(batch, address, key, block_num, value)
    }

    fn commit(&self, batch: Self::DBWriteBatch) -> Result<(), Error> {
        self.db.commit(batch)
    }
}

/// Direct write implementation for Arc<DataBase> without needing a StateDB instance.
/// This is useful for bulk initialization where we don't need read capabilities.
impl StateDBWrite for Arc<DataBase> {
    type DBWriteBatch = MDBXWriteBatch;

    fn prepare_write_batch(&self) -> Result<Self::DBWriteBatch, Error> {
        let txn = self
            .env
            .begin_rw_txn()
            .map_err(|e| Error::UnSupported(format!("Failed to begin write transaction: {}", e)))?;

        // Pre-create cursors only for tables that use immediate writes.
        // AddressToAccount and AddressToStorage use deferred writes (cached and sorted at commit).
        let mut cursors = HashMap::new();
        for table in [
            StorageTable::LatestBlockHash,
            StorageTable::BlockHashToBlockInfo,
            StorageTable::BlockNumToBlockHash,
            StorageTable::HashToCode,
        ] {
            let db = txn
                .open_db(Some(table.to_str()))
                .map_err(|e| Error::UnSupported(format!("Failed to open db {}: {}", table, e)))?;
            let cursor = txn.cursor(&db).map_err(|e| {
                Error::UnSupported(format!("Failed to create cursor for {}: {}", table, e))
            })?;
            cursors.insert(table.to_str(), cursor);
        }

        Ok(MDBXWriteBatch {
            txn,
            cursors,
            account_cache: Vec::new(),
            storage_cache: Vec::new(),
        })
    }

    fn write_latest_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_hash: H256,
    ) -> Result<(), Error> {
        let cursor = batch
            .cursors
            .get_mut(StorageTable::LatestBlockHash.to_str())
            .ok_or_else(|| Error::UnSupported("LatestBlockHash cursor not found".to_string()))?;

        cursor
            .put(
                LATEST_BLOCK_HASH_KEY,
                block_hash.as_slice(),
                WriteFlags::UPSERT,
            )
            .map_err(|e| Error::UnSupported(format!("Failed to write latest block hash: {}", e)))?;
        Ok(())
    }

    fn write_block_info(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_info: BlockInfo,
    ) -> Result<(), Error> {
        let block_info_bytes = to_vec(&block_info)
            .map_err(|e| Error::UnSupported(format!("Failed to serialize block info: {}", e)))?;
        let block_hash = block_info.header.hash;

        let cursor = batch
            .cursors
            .get_mut(StorageTable::BlockHashToBlockInfo.to_str())
            .ok_or_else(|| {
                Error::UnSupported("BlockHashToBlockInfo cursor not found".to_string())
            })?;

        cursor
            .put(block_hash.as_slice(), &block_info_bytes, WriteFlags::UPSERT)
            .map_err(|e| Error::UnSupported(format!("Failed to write block info: {}", e)))?;
        Ok(())
    }

    fn write_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_num: u64,
        block_hash: H256,
    ) -> Result<(), Error> {
        let block_hash_bytes: [u8; 32] = block_hash.into();
        let block_num_bytes = encode_block_num(block_num);

        let cursor = batch
            .cursors
            .get_mut(StorageTable::BlockNumToBlockHash.to_str())
            .ok_or_else(|| {
                Error::UnSupported("BlockNumToBlockHash cursor not found".to_string())
            })?;

        let append_result = cursor.put(&block_num_bytes, &block_hash_bytes, WriteFlags::APPEND);
        if let Err(append_err) = append_result {
            trace!(
                target: "mdbx_archive",
                block_num,
                error = %append_err,
                "APPEND failed for block hash; falling back to UPSERT"
            );
            cursor
                .put(&block_num_bytes, &block_hash_bytes, WriteFlags::UPSERT)
                .map_err(|e| {
                    Error::UnSupported(format!(
                        "Failed to write block hash after append failed ({}): {}",
                        append_err, e
                    ))
                })?;
        }
        Ok(())
    }

    fn write_account(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        block_num: u64,
        raw_account: Option<NewAccount>,
    ) -> Result<(), Error> {
        let key = encode_account_key(address, block_num);
        let value = raw_account.map(encode_slim_account);
        batch.account_cache.push((key, value));
        Ok(())
    }

    fn write_code(
        &self,
        batch: &mut Self::DBWriteBatch,
        code_hash: H256,
        code: Bytes,
    ) -> Result<(), Error> {
        let cursor = batch
            .cursors
            .get_mut(StorageTable::HashToCode.to_str())
            .ok_or_else(|| Error::UnSupported("HashToCode cursor not found".to_string()))?;

        cursor
            .put(code_hash.as_slice(), &code, WriteFlags::UPSERT)
            .map_err(|e| Error::UnSupported(format!("Failed to write code: {}", e)))?;
        Ok(())
    }

    fn write_storage(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        key: H256,
        block_num: u64,
        value: U256,
    ) -> Result<(), Error> {
        let storage_key = encode_storage_key(address, key, block_num);
        let value_bytes: [u8; 32] = value.to_be_bytes();
        batch.storage_cache.push((storage_key, value_bytes));
        Ok(())
    }

    fn commit(&self, mut batch: Self::DBWriteBatch) -> Result<(), Error> {
        // 1. Sort and write cached account data
        if !batch.account_cache.is_empty() {
            // Stable sort by key: equal keys keep insertion order, so the last
            // write per key is naturally the last entry within its run.
            batch.account_cache.sort_by(|a, b| a.0.cmp(&b.0));

            // Create cursor and write sorted data
            let db = batch
                .txn
                .open_db(Some(StorageTable::AddressToAccount.to_str()))
                .map_err(|e| {
                    Error::UnSupported(format!("Failed to open AddressToAccount: {}", e))
                })?;
            let mut cursor = batch.txn.cursor(&db).map_err(|e| {
                Error::UnSupported(format!("Failed to create AddressToAccount cursor: {}", e))
            })?;

            let mut last_write: Option<([u8; 64], Option<Vec<u8>>)> = None;
            for (key, value_opt) in batch.account_cache {
                if let Some((prev_key, prev_value_opt)) = last_write.take() {
                    if prev_key != key {
                        let value = prev_value_opt.as_deref().unwrap_or(&[]);
                        cursor
                            .put(&prev_key, value, WriteFlags::UPSERT)
                            .map_err(|e| {
                                Error::UnSupported(format!("Failed to write account: {}", e))
                            })?;
                    }
                }
                last_write = Some((key, value_opt));
            }
            if let Some((key, value_opt)) = last_write {
                let value = value_opt.as_deref().unwrap_or(&[]);
                cursor
                    .put(&key, value, WriteFlags::UPSERT)
                    .map_err(|e| Error::UnSupported(format!("Failed to write account: {}", e)))?;
            }
        }

        // 2. Sort and write cached storage data
        if !batch.storage_cache.is_empty() {
            // Stable sort by key: equal keys keep insertion order, so the last
            // write per key is naturally the last entry within its run.
            batch.storage_cache.sort_by(|a, b| a.0.cmp(&b.0));

            // Create cursor and write sorted data
            let db = batch
                .txn
                .open_db(Some(StorageTable::AddressToStorage.to_str()))
                .map_err(|e| {
                    Error::UnSupported(format!("Failed to open AddressToStorage: {}", e))
                })?;
            let mut cursor = batch.txn.cursor(&db).map_err(|e| {
                Error::UnSupported(format!("Failed to create AddressToStorage cursor: {}", e))
            })?;

            let mut last_write: Option<([u8; 96], [u8; 32])> = None;
            for (key, value) in batch.storage_cache {
                if let Some((prev_key, prev_value)) = last_write.take() {
                    if prev_key != key {
                        cursor
                            .put(&prev_key, &prev_value, WriteFlags::UPSERT)
                            .map_err(|e| {
                                Error::UnSupported(format!("Failed to write storage: {}", e))
                            })?;
                    }
                }
                last_write = Some((key, value));
            }
            if let Some((key, value)) = last_write {
                cursor
                    .put(&key, &value, WriteFlags::UPSERT)
                    .map_err(|e| Error::UnSupported(format!("Failed to write storage: {}", e)))?;
            }
        }

        // 3. Commit the transaction
        batch
            .txn
            .commit()
            .map_err(|e| Error::UnSupported(format!("Failed to commit transaction: {}", e)))?;
        Ok(())
    }
}
