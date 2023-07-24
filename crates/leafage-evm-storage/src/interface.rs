use auto_impl::auto_impl;
use leafage_evm_types::{BlockDiff, BlockInfo};
use reth_primitives::BlockHashOrNumber;
use revm::db::DatabaseRef;

#[auto_impl(&, Box)]
pub trait EvmStorageRead {
    type Error;
    type StateDB: DatabaseRef<Error = Self::Error>;
    fn state_at(
        &self,
        block_arg: BlockHashOrNumber,
    ) -> Result<Option<(BlockInfo, &Self::StateDB)>, Self::Error>;

    fn block_info_at(
        &self,
        block_arg: BlockHashOrNumber,
    ) -> Result<Option<BlockInfo>, Self::Error> {
        Ok(self.state_at(block_arg)?.map(|(block_info, _)| block_info))
    }
}

#[auto_impl(& mut, Box)]
pub trait EvmStorageWrite {
    type Error;
    fn update_block(
        &mut self,
        block_info: BlockInfo,
        block_diff: BlockDiff,
    ) -> Result<(), Self::Error>;
}
