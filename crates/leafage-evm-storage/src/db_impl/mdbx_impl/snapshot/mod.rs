//! MDBX implementation of the database.
//!
//! Data is stored in the following format:
//! ```text
//! +-------------------------+--------------------------------------+-------------------------+
//! |  LatestBlockHash        |  BlockHashToBlockInfo (only latest)  |  BlockNumToBlockHash    |
//! +-------------------------+--------------------------------------+-------------------------+
//! |  AddressToAccount       |  AddressToStorage                    |  HashToCode             |
//! +-------------------------+--------------------------------------+-------------------------+
//! ```
//! The `LatestBlockHash` table stores the latest block hash.
//! The `BlockHashToBlockInfo` table stores the block hash to block info maps.
//! The `BlockNumToBlockHash` table stores the block number to block hash maps.
//! The `AddressToAccount` table stores the address to account maps.
//! The `AddressToStorage` table stores the (address,index) to storage maps.
//! The `HashToCode` table stores the code hash to code maps.
//! All [`U256`] are big-endian encoded.

use super::{default_page_size, DEFAULT_MAX_READERS, GIGABYTE, MEGABYTE, TERABYTE};
use crate::db::{BlockIterator, LatestStateDBIterator, StateDBProvider, StateDBRead, StateDBWrite};
use crate::db_impl::archive_encoding::{decode_stored_account, encode_stored_account};
use crate::db_impl::error::Error;
use crate::metrics::STORAGE_METRICS;
use leafage_evm_types::{BlockId, BlockInfo, BlockNumberOrTag, Bytes, StoredAccount, H256, U256};
use libmdbx::{
    Cursor, DatabaseFlags, Environment, EnvironmentFlags, Geometry, Mode, PageSize, SyncMode,
    Transaction, WriteFlags, RO, RW,
};
use serde_json::{from_slice, to_vec};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Display;
use std::path::Path;
use std::sync::Arc;
use tracing::info;

const LATEST_BLOCK_HASH_KEY: &[u8] = &[1u8];

/// Space that a read-only transaction can occupy until the warning is emitted.
/// See [`reth_libmdbx::EnvironmentBuilder::set_handle_slow_readers`] for more information.
#[allow(dead_code)]
const MAX_SAFE_READER_SPACE: usize = 10 * GIGABYTE;

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StorageTable {
    LatestBlockHash = 1,
    // block hash -> block info
    BlockHashToBlockInfo = 2,
    // block num -> block hash
    BlockNumToBlockHash = 3,
    // address -> account
    AddressToAccount = 4,
    // address || storage index -> storage
    AddressToStorage = 5,
    // code hash -> code
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
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_str())
    }
}

/// MDBX database configuration options for performance tuning
#[derive(Debug, Clone)]
pub struct MDBXOptions {
    /// Initial database size (default: 256MB to reduce early growth)
    pub initial_size: usize,
    /// Maximum database size (default: 1TB)
    pub max_size: usize,
    /// Growth step when database needs to expand (default: 4GB)
    /// Larger steps reduce the frequency of expensive resize operations
    pub growth_step: usize,
    /// Sync mode for durability vs performance trade-off
    /// - Durable: Full durability, slower writes (fsync on every commit)
    /// - NoMetaSync: Balanced, data safe on system crash, may lose recent transactions
    /// - SafeNoSync: Good balance, data safe on process crash, may lose on system crash
    /// - UtterlyNoSync: Maximum performance, risk of data loss on any crash
    pub sync_mode: SyncMode,
}

impl Default for MDBXOptions {
    fn default() -> Self {
        Self {
            // Start with 256MB to avoid frequent early growth
            initial_size: 256 * MEGABYTE,
            // Max 1TB
            max_size: 1 * TERABYTE,
            // Grow 4GB at a time
            growth_step: 4 * GIGABYTE,
            // Balanced sync mode
            sync_mode: SyncMode::NoMetaSync,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DataBase {
    env: Environment,
    dbis: Arc<HashMap<&'static str, u32>>,
}

unsafe impl Send for DataBase {}
unsafe impl Sync for DataBase {}

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

        // Optimized geometry configuration:
        // - Start with initial size to reduce early growth operations
        // - Use larger growth step (4GB) to reduce frequency of resize operations
        // - shrink_threshold: 0 means never shrink (avoid fragmentation)
        let geometry = Geometry {
            size: Some(options.initial_size..(options.max_size)),
            growth_step: Some(options.growth_step as isize),
            shrink_threshold: Some(0),
            page_size: Some(PageSize::Set(default_page_size())),
        };

        inner_env.set_geometry(geometry);

        // Optimized environment flags:
        // - SafeNoSync: Data is written to OS buffer cache, fsync on commit is skipped
        //   This is safe because MDBX still maintains ACID properties, data loss only
        //   possible on system crash (not process crash)
        // - no_rdahead: Disable OS read-ahead, better for random access patterns
        // - coalesce: Enable free space coalescing
        // - no_meminit: Skip memory initialization for new pages (faster allocation)
        // - liforeclaim: Use LIFO page reclaim to improve locality and reduce fragmentation
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

        // Increase reader page augment limit for better read performance
        inner_env.set_rp_augment_limit(512 * 1024);

        let mut dbis = HashMap::new();

        // Open database
        let env = inner_env
            .open(path)
            .expect("Failed to open MDBX environment");

        info!(target = "mdbx", "Opened MDBX database at {:?}", path);

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
}

// Helper functions to convert cursor iterator results
fn decode_account(
    result: libmdbx::Result<(Cow<'_, [u8]>, Cow<'_, [u8]>)>,
) -> Result<(H256, StoredAccount), Error> {
    let (key, value) = result.map_err(|e| Error::UnSupported(format!("Iterator error: {}", e)))?;
    let address = H256::from_slice(&key);
    let account = decode_stored_account(&value)
        .map_err(|e| Error::UnSupported(format!("Failed to decode account: {}", e)))?;
    Ok((address, account))
}

fn decode_code(
    result: libmdbx::Result<(Cow<'_, [u8]>, Cow<'_, [u8]>)>,
) -> Result<(H256, Bytes), Error> {
    let (key, value) = result.map_err(|e| Error::UnSupported(format!("Iterator error: {}", e)))?;
    Ok((H256::from_slice(&key), Bytes::from(value.to_vec())))
}

fn decode_storage(
    result: libmdbx::Result<(Cow<'_, [u8]>, Cow<'_, [u8]>)>,
) -> Result<(H256, H256, U256), Error> {
    let (key, value) = result.map_err(|e| Error::UnSupported(format!("Iterator error: {}", e)))?;
    if key.len() < 64 {
        return Err(Error::UnSupported("Invalid storage key length".to_string()));
    }
    Ok((
        H256::from_slice(&key[..32]),
        H256::from_slice(&key[32..64]),
        U256::from_be_slice(&value),
    ))
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

// Helper to create cursor for a table
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

impl LatestStateDBIterator for DataBase {
    fn account_iter(&self) -> impl Iterator<Item = Result<(H256, StoredAccount), Error>> {
        match create_cursor(&self.env, StorageTable::AddressToAccount) {
            Ok(cursor) => {
                Box::new(cursor.iter_slices().map(decode_account)) as Box<dyn Iterator<Item = _>>
            }
            Err(e) => Box::new(std::iter::once(Err(e))) as Box<dyn Iterator<Item = _>>,
        }
    }

    fn code_iter(&self) -> impl Iterator<Item = Result<(H256, Bytes), Error>> {
        match create_cursor(&self.env, StorageTable::HashToCode) {
            Ok(cursor) => {
                Box::new(cursor.iter_slices().map(decode_code)) as Box<dyn Iterator<Item = _>>
            }
            Err(e) => Box::new(std::iter::once(Err(e))) as Box<dyn Iterator<Item = _>>,
        }
    }

    fn storage_iter(&self) -> impl Iterator<Item = Result<(H256, H256, U256), Error>> {
        match create_cursor(&self.env, StorageTable::AddressToStorage) {
            Ok(cursor) => {
                Box::new(cursor.iter_slices().map(decode_storage)) as Box<dyn Iterator<Item = _>>
            }
            Err(e) => Box::new(std::iter::once(Err(e))) as Box<dyn Iterator<Item = _>>,
        }
    }
}

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

impl StateDBProvider for Arc<DataBase> {
    type StateDBReadWrite = StateDB;

    fn db_at(&self, block_id: BlockId) -> Result<Option<Self::StateDBReadWrite>, Error> {
        match block_id {
            BlockId::Number(block_number_or_tag) => match block_number_or_tag {
                BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                    let txn = self.env.begin_ro_txn().map_err(|e| {
                        Error::UnSupported(format!("Failed to begin read transaction: {}", e))
                    })?;
                    Ok(Some(StateDB {
                        txn,
                        dbis: self.dbis.clone(),
                    }))
                }
                _ => Ok(None),
            },
            BlockId::Hash(_) => Ok(None),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StateDB {
    txn: Transaction<RO>,
    dbis: Arc<HashMap<&'static str, u32>>,
}

impl StateDBRead for StateDB {
    fn read_block_hash(&self, block_num: u64) -> Result<H256, Error> {
        let start = std::time::Instant::now();

        let dbi = self
            .dbis
            .get(StorageTable::BlockNumToBlockHash.to_str())
            .ok_or_else(|| Error::UnSupported("BlockNumToBlockHash table not found".to_string()))?;
        let block_num_bytes: [u8; 32] = U256::from(block_num).to_be_bytes();

        let block_hash_bytes: Option<Cow<'_, [u8]>> = self
            .txn
            .get(*dbi, &block_num_bytes)
            .map_err(|e| Error::UnSupported(format!("Failed to read block hash: {}", e)))?;

        STORAGE_METRICS
            .read_block_hash_latency
            .record(start.elapsed().as_secs_f64());

        match block_hash_bytes {
            Some(bytes) if !bytes.is_empty() => Ok(H256::from_slice(&bytes)),
            _ => Err(Error::UnSupported("Block not found".to_string())),
        }
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<BlockInfo>, Error> {
        let start = std::time::Instant::now();

        let dbi = self
            .dbis
            .get(StorageTable::BlockHashToBlockInfo.to_str())
            .ok_or_else(|| {
                Error::UnSupported("BlockHashToBlockInfo table not found".to_string())
            })?;
        let block_hash_bytes: [u8; 32] = block_hash.into();
        let block_info_bytes: Option<Cow<'_, [u8]>> = self
            .txn
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

    fn read_latest_block_hash(&self) -> Result<H256, Error> {
        let start = std::time::Instant::now();

        let dbi = self
            .dbis
            .get(StorageTable::LatestBlockHash.to_str())
            .ok_or_else(|| Error::UnSupported("LatestBlockHash table not found".to_string()))?;

        let block_hash_bytes: Option<Cow<'_, [u8]>> = self
            .txn
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

    fn read_account(&self, address: H256) -> Result<Option<StoredAccount>, Error> {
        let start = std::time::Instant::now();

        let dbi = self
            .dbis
            .get(StorageTable::AddressToAccount.to_str())
            .ok_or_else(|| Error::UnSupported("AddressToAccount table not found".to_string()))?;

        let address_bytes: [u8; 32] = address.into();
        let raw_account_bytes: Option<Cow<'_, [u8]>> = self
            .txn
            .get(*dbi, &address_bytes)
            .map_err(|e| Error::UnSupported(format!("Failed to read account: {}", e)))?;

        STORAGE_METRICS
            .read_account_latency
            .record(start.elapsed().as_secs_f64());

        match raw_account_bytes {
            Some(bytes) => {
                let account = decode_stored_account(&bytes)?;
                Ok(Some(account))
            }
            None => Ok(None),
        }
    }

    fn read_storage(&self, address: H256, key: H256) -> Result<U256, Error> {
        let start = std::time::Instant::now();

        let dbi = self
            .dbis
            .get(StorageTable::AddressToStorage.to_str())
            .ok_or_else(|| Error::UnSupported("AddressToStorage table not found".to_string()))?;

        let address_bytes: [u8; 32] = address.into();
        let key_bytes: [u8; 32] = key.into();
        let storage_key = [address_bytes.as_ref(), &key_bytes].concat();

        let value_bytes: Option<Cow<'_, [u8]>> = self
            .txn
            .get(*dbi, &storage_key)
            .map_err(|e| Error::UnSupported(format!("Failed to read storage: {}", e)))?;

        STORAGE_METRICS
            .read_storage_latency
            .record(start.elapsed().as_secs_f64());

        match value_bytes {
            Some(bytes) if !bytes.is_empty() => Ok(U256::from_be_slice(&bytes)),
            _ => Ok(U256::ZERO),
        }
    }

    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, Error> {
        let start = std::time::Instant::now();

        let dbi = self
            .dbis
            .get(StorageTable::HashToCode.to_str())
            .ok_or_else(|| Error::UnSupported("HashToCode table not found".to_string()))?;

        let code_hash_bytes: [u8; 32] = code_hash.into();
        let code: Option<Cow<'_, [u8]>> = self
            .txn
            .get(*dbi, &code_hash_bytes)
            .map_err(|e| Error::UnSupported(format!("Failed to read code: {}", e)))?;

        STORAGE_METRICS
            .read_code_latency
            .record(start.elapsed().as_secs_f64());

        match code {
            Some(bytes) => Ok(Some(Bytes::from(bytes.to_vec()))),
            None => Ok(None),
        }
    }
}

pub struct MDBXWriteBatch {
    txn: Transaction<RW>,
}

impl StateDBWrite for StateDB {
    type DBWriteBatch = MDBXWriteBatch;

    fn prepare_write_batch(&self) -> Result<Self::DBWriteBatch, Error> {
        let txn =
            self.txn.env().begin_rw_txn().map_err(|e| {
                Error::UnSupported(format!("Failed to begin write transaction: {}", e))
            })?;

        Ok(MDBXWriteBatch { txn })
    }

    fn write_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_num: u64,
        block_hash: H256,
    ) -> Result<(), Error> {
        let dbi = self
            .dbis
            .get(StorageTable::BlockNumToBlockHash.to_str())
            .ok_or_else(|| Error::UnSupported("BlockNumToBlockHash table not found".to_string()))?;

        let block_hash_bytes: [u8; 32] = block_hash.into();
        let block_num_bytes: [u8; 32] = U256::from(block_num).to_be_bytes();
        batch
            .txn
            .put(
                *dbi,
                &block_num_bytes,
                &block_hash_bytes,
                WriteFlags::empty(),
            )
            .map_err(|e| Error::UnSupported(format!("Failed to write block hash: {}", e)))?;
        Ok(())
    }

    fn write_block_info(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_info: BlockInfo,
    ) -> Result<(), Error> {
        let dbi = self
            .dbis
            .get(StorageTable::BlockHashToBlockInfo.to_str())
            .ok_or_else(|| {
                Error::UnSupported("BlockHashToBlockInfo table not found".to_string())
            })?;

        // Delete parent block info to keep only the latest
        batch
            .txn
            .del(*dbi, block_info.header.parent_hash.as_slice(), None)
            .ok(); // Ignore error if parent doesn't exist

        let block_info_bytes = to_vec(&block_info)
            .map_err(|e| Error::UnSupported(format!("Failed to serialize block info: {}", e)))?;
        let block_hash = block_info.header.hash;
        batch
            .txn
            .put(
                *dbi,
                block_hash.as_slice(),
                &block_info_bytes,
                WriteFlags::empty(),
            )
            .map_err(|e| Error::UnSupported(format!("Failed to write block info: {}", e)))?;
        Ok(())
    }

    fn write_account(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        _block_num: u64,
        raw_account: Option<StoredAccount>,
    ) -> Result<(), Error> {
        let dbi = self
            .dbis
            .get(StorageTable::AddressToAccount.to_str())
            .ok_or_else(|| Error::UnSupported("AddressToAccount table not found".to_string()))?;
        let address_bytes = address.as_slice();
        if let Some(raw_account) = raw_account {
            let raw_account_bytes = encode_stored_account(raw_account);
            batch
                .txn
                .put(*dbi, address_bytes, &raw_account_bytes, WriteFlags::empty())
                .map_err(|e| Error::UnSupported(format!("Failed to write account: {}", e)))?;
        } else {
            batch.txn.del(*dbi, address_bytes, None).ok(); // Ignore error if not exists
        }
        Ok(())
    }

    fn write_storage(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        key: H256,
        _block_num: u64,
        value: U256,
    ) -> Result<(), Error> {
        let dbi = self
            .dbis
            .get(StorageTable::AddressToStorage.to_str())
            .ok_or_else(|| Error::UnSupported("AddressToStorage table not found".to_string()))?;
        let address_bytes = address.as_slice();
        let key_bytes: [u8; 32] = key.into();
        let storage_key = [address_bytes, &key_bytes].concat();

        if value == U256::ZERO {
            batch.txn.del(*dbi, &storage_key, None).ok(); // Ignore error if not exists
        } else {
            let value_bytes: [u8; 32] = value.to_be_bytes();
            batch
                .txn
                .put(*dbi, &storage_key, &value_bytes, WriteFlags::empty())
                .map_err(|e| Error::UnSupported(format!("Failed to write storage: {}", e)))?;
        }
        Ok(())
    }

    fn write_code(
        &self,
        batch: &mut Self::DBWriteBatch,
        code_hash: H256,
        code: Bytes,
    ) -> Result<(), Error> {
        let dbi = self
            .dbis
            .get(StorageTable::HashToCode.to_str())
            .ok_or_else(|| Error::UnSupported("HashToCode table not found".to_string()))?;

        let code_hash_bytes = code_hash.as_slice();
        batch
            .txn
            .put(*dbi, code_hash_bytes, &code, WriteFlags::empty())
            .map_err(|e| Error::UnSupported(format!("Failed to write code: {}", e)))?;
        Ok(())
    }

    fn write_latest_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_hash: H256,
    ) -> Result<(), Error> {
        let dbi = self
            .dbis
            .get(StorageTable::LatestBlockHash.to_str())
            .ok_or_else(|| Error::UnSupported("LatestBlockHash table not found".to_string()))?;

        batch
            .txn
            .put(*dbi, &[1u8], block_hash.as_slice(), WriteFlags::empty())
            .map_err(|e| Error::UnSupported(format!("Failed to write latest block hash: {}", e)))?;
        Ok(())
    }

    fn commit(&self, batch: Self::DBWriteBatch) -> Result<(), Error> {
        batch
            .txn
            .commit()
            .map_err(|e| Error::UnSupported(format!("Failed to commit transaction: {}", e)))?;
        Ok(())
    }
}
