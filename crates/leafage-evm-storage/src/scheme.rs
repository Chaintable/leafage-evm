use crate::interface::EvmStorageWrite;
use auto_impl::auto_impl;
use leafage_evm_types::{BlockDiff, BlockInfo, RawAccount};
use reth_primitives::{Address, Bytes, H256, U256};
use revm::db::DatabaseRef;
use revm::primitives::{AccountInfo, Bytecode, B160, B256};

#[auto_impl(&, Box)]
pub trait StateDBRead {
    type Error;
    /// latest block hash
    fn read_latest_block_hash(&self) -> Result<Option<H256>, Self::Error>;

    /// block hash -> block info
    fn read_block_info(&self, block_hash: H256, block_info: BlockInfo) -> Result<(), Self::Error>;

    /// block num -> block hash
    fn read_block_num(&self, block_num: U256) -> Result<Option<H256>, Self::Error>;

    /// account address -> raw account
    fn read_account(&self, address: Address) -> Result<Option<RawAccount>, Self::Error>;

    /// code hash -> code
    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, Self::Error>;

    /// account address | storage index -> storage value
    fn read_storage(&self, key: (Address, U256)) -> Result<U256, Self::Error>;
}
#[auto_impl(& , Box)]
pub trait StateDBWrite {
    type Error;
    /// latest block hash
    fn write_latest_block_hash(&self, block_hash: H256) -> Result<(), Self::Error>;

    /// block hash -> block info
    fn write_block_info(&self, block_hash: H256, block_info: BlockInfo) -> Result<(), Self::Error>;

    /// block num -> block hash
    fn write_block_num(&self, block_num: U256, block_hash: H256) -> Result<(), Self::Error>;

    /// account address -> raw account
    fn write_account(
        &self,
        address: Address,
        raw_account: Option<RawAccount>,
    ) -> Result<(), Self::Error>;

    /// code hash -> code
    fn write_code(&self, code_hash: H256, code: Bytes) -> Result<(), Self::Error>;

    /// account address | storage index -> storage value
    fn write_storage(&self, key: (Address, U256), value: U256) -> Result<(), Self::Error>;
}

struct DBWrapper<T>(T);

impl<T> DatabaseRef for DBWrapper<T>
where
    T: StateDBRead,
{
    type Error = T::Error;

    fn basic(&self, address: B160) -> Result<Option<AccountInfo>, Self::Error> {
        let raw_account_info = self.0.read_account(address)?;
        if let Some(mut raw_account_info) = raw_account_info {
            if raw_account_info.code.is_empty() {
                if !raw_account_info.code_hash.is_empty() {
                    let code = self.0.read_code(raw_account_info.code_hash)?;
                    if let Some(code) = code {
                        raw_account_info.code = code;
                    }
                }
            }
            Ok(Some(raw_account_info.into()))
        } else {
            Ok(None)
        }
    }
    /// Get account code by its hash
    fn code_by_hash(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        let code = self.0.read_code(code_hash)?;
        if let Some(code) = code {
            Ok(Bytecode::new_raw(code.into()))
        } else {
            Ok(Bytecode::default())
        }
    }
    /// Get storage value of address at index.
    fn storage(&self, address: B160, index: U256) -> Result<U256, Self::Error> {
        self.0.read_storage((address, index))
    }

    // History related
    fn block_hash(&self, number: U256) -> Result<B256, Self::Error> {
        Ok(self.0.read_block_num(number)?.unwrap_or_default())
    }
}

impl<T> EvmStorageWrite for DBWrapper<T>
where
    T: StateDBWrite,
{
    type Error = T::Error;

    fn update_block(
        &self,
        block_info: BlockInfo,
        block_diff: BlockDiff,
    ) -> Result<(), Self::Error> {
        self.0.write_block_num(block_info.number, block_info.hash)?;
        let hash = block_info.hash;
        self.0.write_block_info(block_info.hash, block_info)?;
        for account in block_diff.accounts_diff {
            if let Some(account) = account.info.as_ref() {
                if !account.code_hash.is_zero() {
                    self.0.write_code(account.code_hash, account.code.clone())?;
                }
            }
            self.0.write_account(account.address, account.info)?;
        }
        for account_diff in block_diff.storage_diff {
            for index_value_pair in account_diff.value {
                self.0.write_storage(
                    (account_diff.account_addr, index_value_pair.index),
                    index_value_pair.value,
                )?;
            }
        }
        self.0.write_latest_block_hash(hash)?;
        Ok(())
    }
}
