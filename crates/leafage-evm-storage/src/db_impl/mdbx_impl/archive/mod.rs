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
use crate::db_impl::error::Error;
use crate::metrics::STORAGE_METRICS;
use alloy_rlp::{Decodable, Encodable};
use leafage_evm_types::{
    Block, BlockId, BlockNumberOrTag, Bytes, Header, NewAccount, SlimAccount, H256,
    KECCAK256_EMPTY, U256,
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
use tracing::info;

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
    block_header: Header,
}

impl Clone for StateDB {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            block_num: self.block_num,
            block_header: self.block_header.clone(),
        }
    }
}

impl Debug for StateDB {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateDB")
            .field("block_num", &self.block_num)
            .field("block_header", &self.block_header)
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

        let block_num_bytes: [u8; 32] = U256::from(block_num).to_be_bytes();

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
    pub fn read_block_info(&self, block_hash: H256) -> Result<Option<Block<H256>>, Error> {
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
                let block_info = from_slice::<Block<H256>>(&bytes)?;
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

// Encode account key: address(32) || block_num(32)
#[inline]
fn encode_account_key(address: H256, block_num: u64) -> [u8; 64] {
    let mut key = [0u8; 64];
    key[..32].copy_from_slice(address.as_slice());
    key[32..64].copy_from_slice(&U256::from(block_num).to_be_bytes::<32>());
    key
}

// Encode storage key: address(32) || storage_key(32) || block_num(32)
#[inline]
fn encode_storage_key(address: H256, storage_key: H256, block_num: u64) -> [u8; 96] {
    let mut key = [0u8; 96];
    key[..32].copy_from_slice(address.as_slice());
    key[32..64].copy_from_slice(storage_key.as_slice());
    key[64..96].copy_from_slice(&U256::from(block_num).to_be_bytes::<32>());
    key
}

/// Simulate seek_for_prev for MDBX cursor.
///
/// Returns the key-value pair where key <= target_key.
/// If no such key exists, returns None.
///
/// # Important
///
/// When no key >= target_key exists, this function returns the **last entry**
/// in the database, which may have a completely different key prefix.
/// **Callers must verify the returned key matches the expected prefix** before
/// using the value. This is by design for archive storage where we need to find
/// the most recent state at or before a given block number.
fn seek_for_prev<'a>(
    cursor: &'a mut Cursor<RO>,
    target_key: &[u8],
) -> Result<Option<(Cow<'a, [u8]>, Cow<'a, [u8]>)>, Error> {
    // Try to set_range to find key >= target_key
    match cursor.set_range::<Cow<'a, [u8]>, Cow<'a, [u8]>>(target_key) {
        Ok(Some((key, value))) => {
            if key.as_ref() == target_key {
                // Exact match
                Ok(Some((key, value)))
            } else {
                // key > target_key, need to go back one step
                cursor
                    .prev::<Cow<'a, [u8]>, Cow<'a, [u8]>>()
                    .map_err(|e| Error::UnSupported(format!("Cursor prev failed: {}", e)))
            }
        }
        Ok(None) => {
            // No key >= target_key found, all keys are < target_key
            // Go to the last entry (caller must verify prefix!)
            cursor
                .last::<Cow<'a, [u8]>, Cow<'a, [u8]>>()
                .map_err(|e| Error::UnSupported(format!("Cursor last failed: {}", e)))
        }
        Err(e) => Err(Error::UnSupported(format!("Set range failed: {}", e))),
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
) -> Result<Block<H256>, Error> {
    let (_, value) = result.map_err(|e| Error::UnSupported(format!("Iterator error: {}", e)))?;
    from_slice::<Block<H256>>(&value)
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
                let iter = cursor.iter_slices();
                let mut iter = iter.peekable();

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

                        // Check if next record has the same address
                        let is_last_for_address = match iter.peek() {
                            Some(Ok((next_key, _))) => {
                                next_key.len() < 32 || next_key[..32] != address_bytes
                            }
                            Some(Err(_)) => true,
                            None => true,
                        };

                        if !is_last_for_address {
                            continue;
                        }

                        let address = H256::from_slice(&address_bytes);

                        // Skip empty values (deleted accounts)
                        if value.is_empty() {
                            continue;
                        }

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
                let iter = cursor.iter_slices();
                let mut iter = iter.peekable();

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

                        // Check if next record has the same (address, storage_key)
                        let is_last_for_prefix = match iter.peek() {
                            Some(Ok((next_key, _))) => {
                                next_key.len() < 64 || next_key[..64] != prefix
                            }
                            Some(Err(_)) => true,
                            None => true,
                        };

                        if !is_last_for_prefix {
                            continue;
                        }

                        let address = H256::from_slice(&prefix[..32]);
                        let storage_key = H256::from_slice(&prefix[32..64]);
                        let storage_value = U256::from_be_slice(&value);

                        // Skip zero values (deleted storage slots)
                        if storage_value == U256::ZERO {
                            continue;
                        }

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
    fn block_info_iter(&self) -> impl Iterator<Item = Result<Block<H256>, Error>> {
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
        let block_header: Header;

        match block_id {
            BlockId::Hash(hash) => {
                let header = self.read_block_info(hash.block_hash)?;
                if header.is_none() {
                    return Ok(None);
                }
                block_header = header.unwrap().header;
                block_num = block_header.number;
            }
            BlockId::Number(block_number_or_tag) => match block_number_or_tag {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                    let latest_block_num = self.read_latest_block_num()?;
                    match latest_block_num {
                        Some(num) => {
                            block_num = num;
                            let block_hash = self.read_block_hash(num)?;
                            if block_hash == H256::ZERO {
                                return Ok(None);
                            }
                            let header = self.read_block_info(block_hash)?;
                            if header.is_none() {
                                return Ok(None);
                            }
                            block_header = header.unwrap().header;
                        }
                        None => {
                            return Ok(None);
                        }
                    }
                }
                BlockNumberOrTag::Number(num) => {
                    block_num = num;
                    let block_hash = self.read_block_hash(num)?;
                    if block_hash == H256::ZERO {
                        return Ok(None);
                    }
                    let header = self.read_block_info(block_hash)?;
                    if header.is_none() {
                        return Ok(None);
                    }
                    block_header = header.unwrap().header;
                }
                _ => return Err(Error::UnsupportedBlockId(block_id)),
            },
        }

        Ok(Some(StateDB {
            db: self.clone(),
            block_num,
            block_header,
        }))
    }
}

// ===== StateDBRead Implementation =====

impl StateDBRead for StateDB {
    fn read_latest_block_hash(&self) -> Result<H256, Error> {
        Ok(self.block_header.hash)
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<Block<H256>>, Error> {
        if block_hash == self.block_header.hash {
            return Ok(Some(Block {
                header: self.block_header.clone(),
                ..Default::default()
            }));
        }
        self.db.read_block_info(block_hash)
    }

    fn read_block_hash(&self, block_num: u64) -> Result<H256, Error> {
        if block_num == self.block_num {
            return Ok(self.block_header.hash);
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

        // Use seek_for_prev to find the latest account state <= block_num
        let result = match seek_for_prev(&mut cursor, &target_key)? {
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

        // Use seek_for_prev to find the latest storage value <= block_num
        let result = match seek_for_prev(&mut cursor, &target_key)? {
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
        block_info: Block<H256>,
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
        block_info: Block<H256>,
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
        let block_num_bytes: [u8; 32] = U256::from(block_num).to_be_bytes();

        let cursor = batch
            .cursors
            .get_mut(StorageTable::BlockNumToBlockHash.to_str())
            .ok_or_else(|| {
                Error::UnSupported("BlockNumToBlockHash cursor not found".to_string())
            })?;

        // Use APPEND mode since block_num is strictly increasing during archive init.
        // This skips B-tree search and directly appends to the end, significantly improving performance.
        cursor
            .put(&block_num_bytes, &block_hash_bytes, WriteFlags::APPEND)
            .map_err(|e| Error::UnSupported(format!("Failed to write block hash: {}", e)))?;
        Ok(())
    }

    fn write_account(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        block_num: u64,
        raw_account: Option<NewAccount>,
    ) -> Result<(), Error> {
        // Key format: address(32) || block_num(32)
        let key = encode_account_key(address, block_num);

        // Encode value and cache for deferred sorted write
        let value = raw_account.map(|acc| {
            let slim_account: SlimAccount = acc.into();
            let mut bytes = Vec::new();
            slim_account.encode(&mut bytes);
            bytes
        });

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
        // Key format: address(32) || storage_key(32) || block_num(32)
        let storage_key = encode_storage_key(address, key, block_num);
        let value_bytes: [u8; 32] = value.to_be_bytes();

        // Cache for deferred sorted write
        batch.storage_cache.push((storage_key, value_bytes));
        Ok(())
    }

    fn commit(&self, mut batch: Self::DBWriteBatch) -> Result<(), Error> {
        // 1. Sort and write cached account data
        if !batch.account_cache.is_empty() {
            // Sort by key (address || block_num) for optimal cursor traversal
            batch.account_cache.sort_unstable_by(|a, b| a.0.cmp(&b.0));

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

            for (key, value_opt) in &batch.account_cache {
                let value = value_opt.as_deref().unwrap_or(&[]);
                cursor.put(key, value, WriteFlags::UPSERT).map_err(|e| {
                    Error::UnSupported(format!("Failed to write account: {}", e))
                })?;
            }
        }

        // 2. Sort and write cached storage data
        if !batch.storage_cache.is_empty() {
            // Sort by key (address || storage_key || block_num) for optimal cursor traversal
            batch.storage_cache.sort_unstable_by(|a, b| a.0.cmp(&b.0));

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

            for (key, value) in &batch.storage_cache {
                cursor.put(key, value, WriteFlags::UPSERT).map_err(|e| {
                    Error::UnSupported(format!("Failed to write storage: {}", e))
                })?;
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
