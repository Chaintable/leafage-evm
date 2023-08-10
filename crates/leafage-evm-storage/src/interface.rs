use auto_impl::auto_impl;
use leafage_evm_types::{BlockDiff, BlockInfo};
use reth_primitives::BlockId;
use revm::db::DatabaseRef;
use revm::primitives::{AccountInfo, Bytecode, B160, B256, U256};

#[auto_impl(&, Box, Arc)]
pub trait StateDB {
    type Error;
    /// Whether account at address exists.
    //fn exists(&self, address: B160) -> Option<AccountInfo>;
    /// Get basic account information.
    fn basic(&self, address: B160) -> Result<Option<AccountInfo>, Self::Error>;
    /// Get account code by its hash
    fn code_by_hash(&self, code_hash: B256) -> Result<Bytecode, Self::Error>;
    /// Get storage value of address at index.
    fn storage(&self, address: B160, index: U256) -> Result<U256, Self::Error>;

    // History related
    fn block_hash(&self, number: U256) -> Result<B256, Self::Error>;
}

#[auto_impl(&, Box, Arc)]
pub trait BlockContext {
    type Error;
    // Block ctx related
    fn block_info(&self) -> Result<BlockInfo, Self::Error>;
}

pub struct WrapDB<T>(T);

impl<T: StateDB> DatabaseRef for WrapDB<T> {
    type Error = T::Error;
    fn basic(&self, address: B160) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.basic(address)
    }
    fn code_by_hash(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.0.code_by_hash(code_hash)
    }
    fn storage(&self, address: B160, index: U256) -> Result<U256, Self::Error> {
        self.0.storage(address, index)
    }
    fn block_hash(&self, number: U256) -> Result<B256, Self::Error> {
        self.0.block_hash(number)
    }
}

#[auto_impl(&, Box)]
pub trait EvmStorageRead {
    type Error;
    type StateDB: StateDB + BlockContext<Error = <Self::StateDB as StateDB>::Error>;
    fn state_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDB>, Self::Error>;
}

#[auto_impl(&, Box)]
pub trait EvmStorageWrite {
    type Error;
    fn update_block(&self, block_info: BlockInfo, block_diff: BlockDiff)
        -> Result<(), Self::Error>;
}
