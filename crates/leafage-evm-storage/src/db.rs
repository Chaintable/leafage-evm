use crate::db_impl::StorageError;
use crate::interface::{BlockContext, EvmStorageWrite, StateDB};
use crate::metrics::STORAGE_METRICS;
use auto_impl::auto_impl;
use leafage_evm_types::{
    AccountInfo, Block, BlockId, BlockStorageDiff, Bytecode, Bytes, NewAccount, H256, U256,
};
use std::fmt::Debug;

#[auto_impl(&, Box, Arc)]
pub trait BlockIterator: Send + Sync + 'static {
    /// block num -> block info
    fn block_info_iter(&self) -> impl Iterator<Item = Result<Block<H256>, StorageError>>;

    /// block num -> block hash
    fn block_hash_iter(&self) -> impl Iterator<Item = Result<(u64, H256), StorageError>>;
}

/// [`StateDBRead`] offers read-only access to the state database.
#[auto_impl(&, Box, Arc)]
pub trait StateDBRead {
    /// latest block hash
    fn read_latest_block_hash(&self) -> Result<H256, StorageError>;

    /// block hash -> block info
    fn read_block_info(&self, block_hash: H256) -> Result<Option<Block<H256>>, StorageError>;

    /// block num -> block hash
    fn read_block_hash(&self, block_num: u64) -> Result<H256, StorageError>;

    /// account address -> raw account
    fn read_account(&self, address: H256) -> Result<Option<NewAccount>, StorageError>;

    /// code hash -> code
    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, StorageError>;

    /// account address | storage index -> storage value
    fn read_storage(&self, address: H256, key: H256) -> Result<U256, StorageError>;
}

#[auto_impl(&, Box, Arc)]
pub trait LatestStateDBIterator: Send + Sync + 'static {
    /// account address -> raw account
    fn account_iter(&self) -> impl Iterator<Item = Result<(H256, NewAccount), StorageError>>;

    /// code hash -> code
    fn code_iter(&self) -> impl Iterator<Item = Result<(H256, Bytes), StorageError>>;

    /// account address | storage index -> storage value
    fn storage_iter(&self) -> impl Iterator<Item = Result<(H256, H256, U256), StorageError>>;
}

/// [`StateDBWrite`] offers write-only access to the state database.
#[auto_impl(& , Box, Arc)]
pub trait StateDBWrite: Send + Sync + 'static {
    type DBWriteBatch: Send;

    /// prepare write batch for write
    fn prepare_write_batch(&self) -> Result<Self::DBWriteBatch, StorageError>;

    /// latest block hash
    fn write_latest_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_hash: H256,
    ) -> Result<(), StorageError>;

    /// block hash -> block info
    fn write_block_info(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_info: Block<H256>,
    ) -> Result<(), StorageError>;

    /// block num -> block hash
    fn write_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_num: u64, // only for archive db
        block_hash: H256,
    ) -> Result<(), StorageError>;

    /// account address -> raw account
    fn write_account(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        block_num: u64, // only for archive db
        raw_account: Option<NewAccount>,
    ) -> Result<(), StorageError>;

    /// code hash -> code
    fn write_code(
        &self,
        batch: &mut Self::DBWriteBatch,
        code_hash: H256,
        code: Bytes,
    ) -> Result<(), StorageError>;

    /// account address | storage index -> storage value
    fn write_storage(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        key: H256,
        block_num: u64,
        value: U256,
    ) -> Result<(), StorageError>;

    /// commit write batch
    fn commit(&self, batch: Self::DBWriteBatch) -> Result<(), StorageError>;
}

/// [`StateDBWrapper`] wraps a [`StateDBRead`] to implements [`BlockContext`]、[`StateDB`] and [`EvmStorageWrite`].
#[derive(Debug)]
pub struct StateDBWrapper<T>(pub T);

impl<T> Clone for StateDBWrapper<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> BlockContext for StateDBWrapper<T>
where
    T: StateDBRead,
{
    type Error = StorageError;

    fn block_info(&self) -> Result<Block<H256>, Self::Error> {
        let latest_block_hash = self.0.read_latest_block_hash()?;
        Ok(self.0.read_block_info(latest_block_hash)?.unwrap())
    }
}

impl<T> StateDB for StateDBWrapper<T>
where
    T: StateDBRead,
{
    type Error = StorageError;

    fn basic(&self, address: H256) -> Result<Option<AccountInfo>, Self::Error> {
        let raw_account_info = self.0.read_account(address.into())?;
        if let Some(raw_account_info) = raw_account_info {
            Ok(Some(raw_account_info.into()))
        } else {
            Ok(None)
        }
    }
    /// Get account code by its hash
    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        let code = self.0.read_code(code_hash.into())?;
        if let Some(code) = code {
            Ok(Bytecode::new_raw(code.0.into()))
        } else {
            Ok(Bytecode::default())
        }
    }
    /// Get storage value of address at index.
    fn storage(&self, address: H256, index: H256) -> Result<U256, Self::Error> {
        self.0.read_storage(address.into(), index)
    }

    // History related
    fn block_hash(&self, number: u64) -> Result<H256, Self::Error> {
        Ok(self.0.read_block_hash(number)?.into())
    }
}

impl<T> EvmStorageWrite for StateDBWrapper<T>
where
    T: StateDBWrite + StateDBRead,
{
    type Error = StorageError;

    fn update_block(
        &self,
        block_info: Block<H256>,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error> {
        let start = std::time::Instant::now();
        let mut batch = self.0.prepare_write_batch()?;
        let block_number = block_info.header.number;
        self.0
            .write_block_hash(&mut batch, block_info.header.number, block_info.header.hash)?;
        let hash = block_info.header.hash;
        self.0.write_block_info(&mut batch, block_info)?;
        for account in block_diff.deleted_accounts {
            self.0
                .write_account(&mut batch, account, block_number, None)?;
        }
        for account in block_diff.new_accounts {
            self.0
                .write_account(&mut batch, account.address, block_number, Some(account))?;
        }
        for account_diff in block_diff.storage_diffs {
            for index_value_pair in account_diff.diffs {
                self.0.write_storage(
                    &mut batch,
                    account_diff.address,
                    index_value_pair.index,
                    block_number,
                    index_value_pair.value,
                )?;
            }
        }
        for new_code in block_diff.new_codes {
            self.0
                .write_code(&mut batch, new_code.code_hash, new_code.code)?;
        }
        self.0.write_latest_block_hash(&mut batch, hash)?;
        self.0.commit(batch)?;
        STORAGE_METRICS
            .commit_block_latency
            .record(start.elapsed().as_secs_f64());
        STORAGE_METRICS.latest_commit_block.set(block_number as f64);
        Ok(())
    }

    fn last_committed_block(&self) -> Result<Option<Block<H256>>, Self::Error> {
        let latest_block_hash = self.0.read_latest_block_hash()?;
        Ok(self.0.read_block_info(latest_block_hash)?)
    }
}

/// [`StateDBProvider`] offers read-only access to the archive database.
#[auto_impl(&, Box, Arc)]
pub trait StateDBProvider: Send + Sync + 'static {
    type StateDBReadWrite: StateDBRead + StateDBWrite + Send + Sync + Clone + Debug + 'static;
    fn db_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDBReadWrite>, StorageError>;

    fn state_at(
        &self,
        block_arg: BlockId,
    ) -> Result<Option<StateDBWrapper<Self::StateDBReadWrite>>, StorageError> {
        let db = self.db_at(block_arg)?;
        Ok(db.map(|db| StateDBWrapper(db)))
    }
}
