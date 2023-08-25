use auto_impl::auto_impl;
use leafage_evm_types::{
    AccountInfo, BlockId, BlockInfo, BlockStorageDiff, Bytecode, H160, H256, U256,
};
use revm::db::DatabaseRef;
use revm::primitives::{B160, B256, U256 as RU256};

/// [`StateDB`] is a trait that provides access to the state of the EVM at a specific block height.
#[auto_impl(&, Box, Arc)]
pub trait StateDB {
    type Error: std::error::Error + Send + Sync + 'static;
    /// Get basic account information.
    fn basic(&self, address: H160) -> Result<Option<AccountInfo>, Self::Error>;
    /// Get account code by its hash
    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error>;
    /// Get storage value of address at index.
    fn storage(&self, address: H160, index: U256) -> Result<U256, Self::Error>;
    // History related
    fn block_hash(&self, number: U256) -> Result<H256, Self::Error>;
}

/// [`BlockContext`] is a trait that provides access to the block information at a specific block height.
#[auto_impl(&, Box, Arc)]
pub trait BlockContext {
    type Error: std::error::Error + Send + Sync + 'static;
    // Block ctx related
    fn block_info(&self) -> Result<BlockInfo, Self::Error>;
}

/// [`WrapDB`] is a wrapper for [`StateDB`] to implement [`DatabaseRef`].
pub struct WrapDB<T>(pub T);

impl<T: StateDB> DatabaseRef for WrapDB<T> {
    type Error = T::Error;
    fn basic(&self, address: B160) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.basic(address.into())
    }
    fn code_by_hash(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.0.code_by_hash(code_hash.into())
    }
    fn storage(&self, address: B160, index: RU256) -> Result<RU256, Self::Error> {
        self.0
            .storage(address.into(), index.into())
            .map(|n| n.into())
    }
    fn block_hash(&self, number: RU256) -> Result<B256, Self::Error> {
        self.0.block_hash(number.into()).map(|h| h.into())
    }
}

/// [`EvmStorageRead`] is a trait that provides specific [`StateDB`] at specific block height.
#[auto_impl(&, Box, Arc)]
pub trait EvmStorageRead {
    type Error: std::error::Error + Send + Sync + 'static;
    type StateDB: StateDB + BlockContext<Error = <Self::StateDB as StateDB>::Error> + Send + Sync;
    fn state_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDB>, Self::Error>;
}

/// [`EvmStorageWrite`] is a trait that provides write access to the undering storage.
#[auto_impl(&, Box)]
pub trait EvmStorageWrite {
    type Error: std::error::Error + Send + Sync + 'static;
    fn update_block(
        &self,
        block_info: BlockInfo,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error>;
}
