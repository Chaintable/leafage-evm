use crate::interface::{BlockContext, EvmStorageWrite, StateDB};
use auto_impl::auto_impl;
use leafage_evm_types::{
    AccountInfo, Block, BlockStorageDiff, Bytecode, Bytes, NewAccount, Transaction, H256, U256,
};

/// [`StateDBRead`] offers read-only access to the state database.
#[auto_impl(&, Box, Arc)]
pub trait StateDBRead: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    /// latest block hash
    fn read_latest_block_hash(&self) -> Result<H256, Self::Error>;

    /// block hash -> block info
    fn read_block_info(&self, block_hash: H256) -> Result<Option<Block<Transaction>>, Self::Error>;

    /// block num -> block hash
    fn read_block_hash(&self, block_num: u64) -> Result<H256, Self::Error>;

    /// account address -> raw account
    fn read_account(&self, address: H256) -> Result<Option<NewAccount>, Self::Error>;

    /// code hash -> code
    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, Self::Error>;

    /// account address | storage index -> storage value
    fn read_storage(&self, address: H256, key: H256) -> Result<U256, Self::Error>;
}

/// [`StateDBWrite`] offers write-only access to the state database.
#[auto_impl(& , Box, Arc)]
pub trait StateDBWrite: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    type DBWriteBatch: Send;

    /// prepare write batch for write
    fn prepare_write_batch(&self) -> Result<Self::DBWriteBatch, Self::Error>;

    /// latest block hash
    fn write_latest_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_hash: H256,
    ) -> Result<(), Self::Error>;

    /// block hash -> block info
    fn write_block_info(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_info: Block<Transaction>,
    ) -> Result<(), Self::Error>;

    /// block num -> block hash
    fn write_block_hash(
        &self,
        batch: &mut Self::DBWriteBatch,
        block_num: u64,
        block_hash: H256,
    ) -> Result<(), Self::Error>;

    /// account address -> raw account
    fn write_account(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        raw_account: Option<NewAccount>,
    ) -> Result<(), Self::Error>;

    /// code hash -> code
    fn write_code(
        &self,
        batch: &mut Self::DBWriteBatch,
        code_hash: H256,
        code: Bytes,
    ) -> Result<(), Self::Error>;

    /// account address | storage index -> storage value
    fn write_storage(
        &self,
        batch: &mut Self::DBWriteBatch,
        address: H256,
        key: H256,
        value: U256,
    ) -> Result<(), Self::Error>;

    /// commit write batch
    fn commit(&self, batch: Self::DBWriteBatch) -> Result<(), Self::Error>;
}

/// [`DBWrapper`] wraps a [`StateDBRead`] to implements [`BlockContext`]、[`StateDB`] and [`EvmStorageWrite`].
pub struct DBWrapper<T>(pub T);

impl<T> BlockContext for DBWrapper<T>
where
    T: StateDBRead,
{
    type Error = T::Error;

    fn block_info(&self) -> Result<Block<Transaction>, Self::Error> {
        let latest_block_hash = self.0.read_latest_block_hash()?;
        Ok(self.0.read_block_info(latest_block_hash)?.unwrap())
    }
}

impl<T> StateDB for DBWrapper<T>
where
    T: StateDBRead,
{
    type Error = T::Error;

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

impl<T> EvmStorageWrite for DBWrapper<T>
where
    T: StateDBWrite,
{
    type Error = T::Error;

    fn update_block(
        &self,
        block_info: Block<Transaction>,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error> {
        let mut batch = self.0.prepare_write_batch()?;
        self.0.write_block_hash(
            &mut batch,
            block_info.header.number.unwrap(),
            block_info.header.hash.unwrap(),
        )?;
        let hash = block_info.header.hash.unwrap();
        self.0.write_block_info(&mut batch, block_info)?;
        for account in block_diff.deleted_accounts {
            self.0.write_account(&mut batch, account, None)?;
        }
        for account in block_diff.new_accounts {
            self.0
                .write_account(&mut batch, account.address, Some(account))?;
        }
        for account_diff in block_diff.storage_diffs {
            for index_value_pair in account_diff.diffs {
                self.0.write_storage(
                    &mut batch,
                    account_diff.address,
                    index_value_pair.index,
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
        Ok(())
    }
}
