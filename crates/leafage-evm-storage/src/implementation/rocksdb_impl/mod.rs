use crate::interface::{BlockContext, EvmStorageWrite, StateDB};
use alloy_rlp::{Decodable, Encodable};
use leafage_evm_types::{
    AccountInfo, BlockInfo, BlockStorageDiff, Bytecode, Bytes, NewAccount, H160, H256, U256,
};
use rocksdb::{
    BlockBasedOptions, Cache, ColumnFamilyDescriptor, Error, Options, WriteBatch,
    WriteBatchWithTransaction, DB,
};
use std::path::Path;

#[repr(u16)]
#[derive(Debug)]
pub enum StorageTypeColumn {
    LatestBlockHash,
    // block hash -> block info
    BlockHashToBlockInfo,
    // block num -> block hash
    BlockNumToBlockHash,
    // address -> account
    AddressToAccount,
    // address || storage index -> storage
    AddressToStorage,
    // code hash -> code
    HashToCode,
}

impl StorageTypeColumn {
    fn to_str(&self) -> &'static str {
        match self {
            StorageTypeColumn::LatestBlockHash => "1",
            StorageTypeColumn::BlockHashToBlockInfo => "2",
            StorageTypeColumn::BlockNumToBlockHash => "3",
            StorageTypeColumn::AddressToAccount => "4",
            StorageTypeColumn::AddressToStorage => "5",
            StorageTypeColumn::HashToCode => "6",
        }
    }
}

pub struct DataBase {
    db: DB,
}

impl DataBase {
    pub fn open<P: AsRef<Path>>(path: P) -> Self {
        let mut cf_opts = Options::default();
        cf_opts.set_max_write_buffer_number(16);
        cf_opts.create_if_missing(true);
        let latest_block_hash_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::LatestBlockHash.to_str(),
            cf_opts.clone(),
        );
        let block_hash_to_block_info_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::BlockHashToBlockInfo.to_str(),
            cf_opts.clone(),
        );
        let block_num_to_block_hash_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::BlockNumToBlockHash.to_str(),
            cf_opts.clone(),
        );
        let address_to_account_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToAccount.to_str(),
            cf_opts.clone(),
        );
        let address_to_storage_cf = ColumnFamilyDescriptor::new(
            StorageTypeColumn::AddressToStorage.to_str(),
            cf_opts.clone(),
        );
        let hash_to_code_cf =
            ColumnFamilyDescriptor::new(StorageTypeColumn::HashToCode.to_str(), cf_opts.clone());
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        db_opts.set_max_write_buffer_number(32);
        let mut block_opts = BlockBasedOptions::default();
        block_opts.set_bloom_filter(10.0, false);
        let cache = Cache::new_lru_cache(1 << 30); // e.g., 1GB
        block_opts.set_block_cache(&cache);
        db_opts.set_block_based_table_factory(&block_opts);
        db_opts.set_write_buffer_size(1 << 30); // e.g., 1GB
        let cfs = vec![
            latest_block_hash_cf,
            block_hash_to_block_info_cf,
            block_num_to_block_hash_cf,
            address_to_account_cf,
            address_to_storage_cf,
            hash_to_code_cf,
        ];
        let db = DB::open_cf_descriptors(&db_opts, path, cfs).unwrap();
        Self { db }
    }

    fn write_block_info(
        &self,
        write_batch: &mut WriteBatchWithTransaction<false>,
        block_info: BlockInfo,
    ) -> Result<(), Error> {
        let block_hash_to_block_info_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
            .unwrap();
        let block_num_to_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .unwrap();

        let block_hash_bytes = block_info.hash.as_ref().as_bytes();
        let block_num_bytes = block_info.number.as_le_bytes();
        let mut block_info_bytes = Vec::new();
        block_info.encode(&mut block_info_bytes);
        write_batch.put_cf(
            block_num_to_block_hash_cf,
            block_num_bytes,
            block_hash_bytes,
        );
        write_batch.put_cf(
            block_hash_to_block_info_cf,
            block_hash_bytes,
            block_info_bytes,
        );
        Ok(())
    }

    fn read_block_info(&self, block_hash: H256) -> Result<Option<BlockInfo>, Error> {
        let block_hash_to_block_info_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockHashToBlockInfo.to_str())
            .unwrap();
        let block_hash_bytes = block_hash.as_ref().as_bytes();
        let block_info_bytes = self
            .db
            .get_cf(block_hash_to_block_info_cf, block_hash_bytes)?;
        if block_info_bytes.is_none() {
            return Ok(None);
        }
        let block_info_bytes = block_info_bytes.unwrap();
        let mut block_info_slice = block_info_bytes.as_slice();
        let block_info = BlockInfo::decode(&mut block_info_slice).unwrap();
        Ok(Some(block_info))
    }

    fn read_block_num(&self, block_num: U256) -> Result<H256, Error> {
        let block_num_to_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::BlockNumToBlockHash.to_str())
            .unwrap();
        let block_num_bytes = block_num.as_le_bytes();
        let block_hash_bytes = self
            .db
            .get_cf(block_num_to_block_hash_cf, block_num_bytes)?;
        if block_hash_bytes.is_none() {
            return Ok(H256::zero());
        }
        let block_hash_bytes = block_hash_bytes.unwrap();
        let block_hash = H256::from_slice(block_hash_bytes.as_slice());
        Ok(block_hash)
    }

    fn write_account(
        &self,
        write_batch: &mut WriteBatchWithTransaction<false>,
        address: H160,
        raw_account: Option<NewAccount>,
    ) -> Result<(), Error> {
        let address_to_account_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
            .unwrap();
        let address_bytes = address.as_ref().as_bytes();
        if let Some(raw_account) = raw_account {
            let mut raw_account_bytes = Vec::new();
            raw_account.encode(&mut raw_account_bytes);
            write_batch.put_cf(address_to_account_cf, address_bytes, raw_account_bytes);
        } else {
            write_batch.delete_cf(address_to_account_cf, address_bytes);
        }
        Ok(())
    }

    fn read_account(&self, address: H160) -> Result<Option<NewAccount>, Error> {
        let address_to_account_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToAccount.to_str())
            .unwrap();
        let address_bytes = address.as_ref().as_bytes();
        let raw_account_bytes = self.db.get_cf(address_to_account_cf, address_bytes)?;
        if raw_account_bytes.is_none() {
            return Ok(None);
        }
        let raw_account_bytes = raw_account_bytes.unwrap();
        let mut raw_account_slice = raw_account_bytes.as_slice();
        let raw_account = NewAccount::decode(&mut raw_account_slice).unwrap();
        Ok(Some(raw_account))
    }

    fn write_storage(
        &self,
        write_batch: &mut WriteBatchWithTransaction<false>,
        address: H160,
        key: U256,
        value: U256,
    ) -> Result<(), Error> {
        let address_to_storage_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
            .unwrap();
        let address_bytes = address.as_ref().as_bytes();
        let key_bytes = key.as_le_slice();
        let key_bytes = [address_bytes, key_bytes].concat();
        let value_bytes = value.as_le_bytes();
        write_batch.put_cf(address_to_storage_cf, key_bytes, value_bytes);
        Ok(())
    }

    fn read_storage(&self, address: H160, key: U256) -> Result<U256, Error> {
        let address_to_storage_cf = self
            .db
            .cf_handle(StorageTypeColumn::AddressToStorage.to_str())
            .unwrap();
        let address_bytes = address.as_ref().as_bytes();
        let key_bytes = key.as_le_slice();
        let key_bytes = [address_bytes, key_bytes].concat();
        let value_bytes = self.db.get_cf(address_to_storage_cf, key_bytes)?;
        if value_bytes.is_none() {
            return Ok(U256::ZERO);
        }
        let value_bytes = value_bytes.unwrap();
        let value_array: [u8; U256::BYTES] = value_bytes.as_slice().try_into().unwrap();
        let value = U256::from_le_bytes(value_array);
        Ok(value)
    }

    fn write_code(
        &self,
        write_batch: &mut WriteBatchWithTransaction<false>,
        code_hash: H256,
        code: Bytes,
    ) -> Result<(), Error> {
        let address_to_code_cf = self
            .db
            .cf_handle(StorageTypeColumn::HashToCode.to_str())
            .unwrap();
        let code_hash_bytes = code_hash.as_ref().as_bytes();
        write_batch.put_cf(address_to_code_cf, code_hash_bytes, code);
        Ok(())
    }

    fn read_code(&self, code_hash: H256) -> Result<Option<Bytes>, Error> {
        let address_to_code_cf = self
            .db
            .cf_handle(StorageTypeColumn::HashToCode.to_str())
            .unwrap();
        let code_hash_bytes = code_hash.as_ref().as_bytes();
        let code = self.db.get_cf(address_to_code_cf, code_hash_bytes)?;
        if code.is_none() {
            return Ok(None);
        }
        Ok(Some(Bytes::from(code.unwrap())))
    }

    fn write_latest_block_hash(
        &self,
        write_batch: &mut WriteBatchWithTransaction<false>,
        block_hash: H256,
    ) -> Result<(), Error> {
        let latest_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
            .unwrap();
        write_batch.put_cf(
            latest_block_hash_cf,
            [1u8].to_vec(),
            block_hash.as_ref().as_bytes(),
        );
        Ok(())
    }

    fn read_latest_block_hash(&self) -> Result<Option<H256>, Error> {
        let latest_block_hash_cf = self
            .db
            .cf_handle(StorageTypeColumn::LatestBlockHash.to_str())
            .unwrap();
        let block_hash_bytes = self.db.get_cf(latest_block_hash_cf, [1u8].to_vec())?;
        if block_hash_bytes.is_none() {
            return Ok(None);
        }
        let block_hash_bytes = block_hash_bytes.unwrap();
        let block_hash = H256::from_slice(block_hash_bytes.as_slice());
        Ok(Some(block_hash))
    }
}

impl EvmStorageWrite for DataBase {
    type Error = Error;

    fn update_block(
        &self,
        block_info: BlockInfo,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error> {
        let mut write_batch = WriteBatch::default();
        let hash = block_info.hash;
        self.write_block_info(&mut write_batch, block_info)?;
        for account in block_diff.new_accounts {
            if !account.code_hash.as_ref().is_zero() {
                self.write_code(&mut write_batch, account.code_hash, account.code.clone())?;
            }
            self.write_account(&mut write_batch, account.address, Some(account))?;
        }
        for address in block_diff.deleted_accounts {
            self.write_account(&mut write_batch, address, None)?;
        }
        for account_diff in block_diff.storage_diff {
            for index_value_pair in account_diff.value {
                self.write_storage(
                    &mut write_batch,
                    account_diff.account_addr,
                    index_value_pair.index,
                    index_value_pair.value,
                )?;
            }
        }
        self.write_latest_block_hash(&mut write_batch, hash)?;
        self.db.write(write_batch)?;
        Ok(())
    }
}

impl StateDB for DataBase {
    type Error = Error;

    fn basic(&self, address: H160) -> Result<Option<AccountInfo>, Self::Error> {
        let raw_account = self.read_account(address)?.map(|a| a.into());
        Ok(raw_account)
    }

    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        let code = self.read_code(code_hash)?;
        Ok(Bytecode::new_raw(code.unwrap_or_default().into()))
    }
    /// Get storage value of address at index.
    fn storage(&self, address: H160, index: U256) -> Result<U256, Self::Error> {
        let value = self.read_storage(address, index)?;
        Ok(value)
    }

    // History related
    fn block_hash(&self, number: U256) -> Result<H256, Self::Error> {
        let block_hash = self.read_block_num(number)?;
        Ok(block_hash)
    }
}

impl BlockContext for DataBase {
    type Error = Error;

    // block_info is the block info of the current block
    fn block_info(&self) -> Result<BlockInfo, Self::Error> {
        let block_hash = self.read_latest_block_hash()?;
        let block_hash = block_hash.unwrap();
        let block_info = self.read_block_info(block_hash)?.unwrap();
        Ok(block_info)
    }
}
