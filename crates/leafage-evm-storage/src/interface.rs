use alloy::primitives::keccak256;
use auto_impl::auto_impl;
use leafage_evm_types::{
    AccountInfo, Address, Block, BlockId, BlockStorageDiff, Bytecode, Transaction, H256, U256,
};
use revm::database_interface::DBErrorMarker;
use revm::DatabaseRef;
use std::sync::Arc;

/// [`StateDB`] is a trait that provides access to the state of the EVM at a specific block height.
#[auto_impl(&, Box, Arc)]
pub trait StateDB {
    type Error: std::error::Error + DBErrorMarker + Send + Sync + 'static;
    /// Get basic account information.
    fn basic(&self, address: H256) -> Result<Option<AccountInfo>, Self::Error>;
    /// Get account code by its hash
    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error>;
    /// Get storage value of address at index.
    fn storage(&self, address: H256, index: H256) -> Result<U256, Self::Error>;
    // History related
    fn block_hash(&self, number: u64) -> Result<H256, Self::Error>;
}

/// [`BlockContext`] is a trait that provides access to the block information at a specific block height.
#[auto_impl(&, Box, Arc)]
pub trait BlockContext {
    type Error: std::error::Error + Send + Sync + 'static;
    // Block ctx related
    fn block_info(&self) -> Result<Block<Transaction>, Self::Error> {
        Ok(self.block_info_arc()?.as_ref().clone())
    }

    fn block_info_arc(&self) -> Result<Arc<Block<Transaction>>, Self::Error> {
        Ok(Arc::new(self.block_info()?))
    }

    fn state_diff(&self) -> Result<BlockStorageDiff, Self::Error> {
        Ok(self.state_diff_arc()?.as_ref().clone())
    }

    fn state_diff_arc(&self) -> Result<Arc<BlockStorageDiff>, Self::Error> {
        Ok(Arc::new(self.state_diff()?))
    }
}

#[derive(Clone, Debug)]
pub struct TxContext {
    pub block_hash: H256,
    pub block_number: u64,
    pub transaction_index: u64,
    pub transaction_hash: H256,
}

/// [`BlockIndex`] is a trait that provides access to the block information at a specific block height.
#[auto_impl(&, Box, Arc)]
pub trait BlockIndex {
    type Error: std::error::Error + Send + Sync + 'static;

    fn get_block_by_id(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Block<Transaction>>, Self::Error> {
        self.get_block_by_id_arc(block_id)
            .map(|b| b.map(|b| b.as_ref().clone()))
    }

    fn get_block_by_id_arc(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Arc<Block<Transaction>>>, Self::Error> {
        self.get_block_by_id(block_id)
            .map(|b| b.map(|b| Arc::new(b)))
    }
}

/// [`EvmStorageWrapper`] is a wrapper for [`StateDB`] to implement [`DatabaseRef`].
#[derive(Clone, Debug)]
pub struct EvmStorageWrapper<T> {
    pub db: T,
    pub using_ovm: bool,
}

impl<T: StateDB> DatabaseRef for EvmStorageWrapper<T> {
    type Error = T::Error;
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let address = if self.using_ovm {
            get_ovm_balance_key(address)
        } else {
            keccak256(address.as_slice())
        };
        self.db.basic(address.into())
    }
    fn code_by_hash_ref(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        self.db.code_by_hash(code_hash.0.into())
    }
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let address = keccak256(address.as_slice());
        let index = keccak256::<[u8; 32]>(index.to_be_bytes());
        self.db
            .storage(address.into(), index.into())
            .map(|n| n.into())
    }
    fn block_hash_ref(&self, number: u64) -> Result<H256, Self::Error> {
        self.db.block_hash(number).map(|h| h.0.into())
    }
}

/// Calculates the OVM storage key for a balance, replicating the logic
/// from the Go function `GetOVMBalanceKey`.
///
/// In the EVM, the storage address for a mapping entry `mapping(key => value)`
/// located at storage slot `p` is computed as `keccak256(padded_key . padded_p)`.
/// This function assumes the storage slot `p` is 0.
///
/// # Arguments
///
/// * `addr` - The H160 (20-byte) address for which to find the balance key.
///
/// # Returns
///
/// * An H256 (32-byte) hash representing the storage key.
pub fn get_ovm_balance_key(addr: Address) -> H256 {
    // 1. Prepare the address. The `key` in the mapping is the user's address.
    //    It must be left-padded with zeros to a full 32 bytes.
    let mut padded_addr = [0u8; 32];
    padded_addr[12..].copy_from_slice(addr.as_slice());

    // 2. Prepare the storage slot position. The Go function uses `common.Big0`,
    //    which is a big integer of value 0. When padded to 32 bytes, this is
    //    simply 32 zero bytes.
    let position_slot = [0u8; 32];

    // 3. Concatenate the padded address and the position slot into a single
    //    64-byte array. The `keccak256` function expects a single byte slice.
    let mut concatenated_data = [0u8; 64];
    concatenated_data[..32].copy_from_slice(&padded_addr);
    concatenated_data[32..].copy_from_slice(&position_slot);

    // 4. Compute the Keccak-256 hash of the concatenated data. This function
    //    returns an alloy_primitives::B256 type.
    keccak256(&concatenated_data)
}

/// [`EvmStorageRead`] is a trait that provides specific [`StateDB`] at specific block height.
#[auto_impl(&, Box, Arc)]
pub trait EvmStorageRead {
    type Error: std::error::Error + Send + Sync + 'static;
    type StateDB: StateDB
        + BlockContext<Error = <Self::StateDB as StateDB>::Error>
        + Send
        + Sync
        + Clone
        + 'static;
    fn state_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDB>, Self::Error>;
}

/// [`EvmStorageWrite`] is a trait that provides write access to the undering storage.
#[auto_impl(&, Box, Arc)]
pub trait EvmStorageWrite {
    type Error: std::error::Error + Send + Sync + 'static;
    fn update_block(
        &self,
        block_info: Block<Transaction>,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error>;

    fn last_committed_block(&self) -> Result<Option<Block<Transaction>>, Self::Error>;
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_get_ovm_balance_key() {
        let address = Address::from_str("0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97").unwrap();
        let expected_key =
            H256::from_str("0x77a8c892f7ab5d424f9ebeccb8fdb4bac5c6eeca52b73ecd947e8d0dc1f81ad9")
                .unwrap();
        assert_eq!(get_ovm_balance_key(address), expected_key);
    }
}
