use crate::{
    db::{ArchiveDBProvider, ArchiveDBWrapper, BlockRead, StateDBWrapper},
    interface::{
        BlockContext, BlockIndex, EvmStorageRead, EvmStorageWrite, StateDB, TransactionIndex,
        TxContext,
    },
    snapshot::{self, LinkedDiffLayer, SnapshotTree, SnapshotTreeConfig},
    StateDBRead, StateDBWrite,
};
use leafage_evm_types::{
    AccountInfo, Block, BlockId, BlockNumberOrTag, BlockStorageDiff, Bytecode, Transaction, H256,
    U256,
};
use std::sync::Arc;
use thiserror::Error;

pub struct ArchiveTree<DB>
where
    DB: ArchiveDBProvider + Sync + Send + 'static,
{
    snapshot_tree: Arc<SnapshotTree<StateDBWrapper<DB::StateDBReadWrite>>>,
    history_tree: ArchiveDBWrapper<DB>,
}

impl<DB> ArchiveTree<DB>
where
    DB: ArchiveDBProvider + Sync + Send + 'static,
{
    pub fn new(
        db: DB,
        config: SnapshotTreeConfig,
    ) -> Result<Self, Error<<DB::StateDBReadWrite as StateDBRead>::Error>> {
        let latest_readwrite_db = db.db_at(BlockId::Number(BlockNumberOrTag::Latest))?;
        let latest_readwrite_db = latest_readwrite_db.expect("latest db should exist");
        let latest_statedb = StateDBWrapper(latest_readwrite_db);
        let snapshot_tree = Arc::new(SnapshotTree::new(latest_statedb, config)?);
        let history_tree = ArchiveDBWrapper(db);
        let tree = Self {
            snapshot_tree: snapshot_tree.clone(),
            history_tree,
        };
        Ok(tree)
    }
}

#[derive(Debug, Error)]
pub enum Error<E> {
    #[error("Snapshot error: {0}")]
    Snapshot(#[from] snapshot::Error<E>),
    #[error("Archive error: {0}")]
    Archive(#[from] E),
}

pub enum MultiStateDB<DB>
where
    DB: ArchiveDBProvider + Sync + Send + 'static,
{
    Snapshot(Arc<LinkedDiffLayer<StateDBWrapper<DB::StateDBReadWrite>>>),
    Archive(StateDBWrapper<DB::StateDBReadWrite>),
}

impl<DB> Clone for MultiStateDB<DB>
where
    DB: ArchiveDBProvider + Sync + Send + 'static,
{
    fn clone(&self) -> Self {
        match self {
            MultiStateDB::Snapshot(s) => MultiStateDB::Snapshot(s.clone()),
            MultiStateDB::Archive(s) => MultiStateDB::Archive(s.clone()),
        }
    }
}

impl<DB> BlockContext for MultiStateDB<DB>
where
    DB: ArchiveDBProvider + Sync + Send + 'static,
{
    type Error = Error<<DB::StateDBReadWrite as StateDBRead>::Error>;

    fn block_info(&self) -> Result<Block<Transaction>, Self::Error> {
        match self {
            MultiStateDB::Snapshot(s) => s.block_info().map_err(Error::Snapshot),
            MultiStateDB::Archive(s) => s.block_info().map_err(Error::Archive),
        }
    }

    fn block_info_arc(&self) -> Result<Arc<Block<Transaction>>, Self::Error> {
        match self {
            MultiStateDB::Snapshot(s) => s.block_info_arc().map_err(Error::Snapshot),
            MultiStateDB::Archive(s) => s.block_info_arc().map_err(Error::Archive),
        }
    }

    fn state_diff(&self) -> Result<BlockStorageDiff, Self::Error> {
        match self {
            MultiStateDB::Snapshot(s) => s.state_diff().map_err(Error::Snapshot),
            MultiStateDB::Archive(s) => s.state_diff().map_err(Error::Archive),
        }
    }

    fn state_diff_arc(&self) -> Result<Arc<BlockStorageDiff>, Self::Error> {
        match self {
            MultiStateDB::Snapshot(s) => s.state_diff_arc().map_err(Error::Snapshot),
            MultiStateDB::Archive(s) => s.state_diff_arc().map_err(Error::Archive),
        }
    }
}

impl<DB> StateDB for MultiStateDB<DB>
where
    DB: ArchiveDBProvider + Sync + Send + 'static,
{
    type Error = Error<<DB::StateDBReadWrite as StateDBRead>::Error>;

    fn basic(&self, address: H256) -> Result<Option<AccountInfo>, Self::Error> {
        match self {
            MultiStateDB::Snapshot(s) => s.basic(address).map_err(Error::Snapshot),
            MultiStateDB::Archive(s) => s.basic(address).map_err(Error::Archive),
        }
    }

    fn code_by_hash(&self, code_hash: H256) -> Result<Bytecode, Self::Error> {
        match self {
            MultiStateDB::Snapshot(s) => s.code_by_hash(code_hash).map_err(Error::Snapshot),
            MultiStateDB::Archive(s) => s.code_by_hash(code_hash).map_err(Error::Archive),
        }
    }

    fn storage(&self, address: H256, index: H256) -> Result<U256, Self::Error> {
        match self {
            MultiStateDB::Snapshot(s) => s.storage(address, index).map_err(Error::Snapshot),
            MultiStateDB::Archive(s) => s.storage(address, index).map_err(Error::Archive),
        }
    }

    fn block_hash(&self, number: u64) -> Result<H256, Self::Error> {
        match self {
            MultiStateDB::Snapshot(s) => s.block_hash(number).map_err(Error::Snapshot),
            MultiStateDB::Archive(s) => s.block_hash(number).map_err(Error::Archive),
        }
    }
}

impl<DB> EvmStorageRead for ArchiveTree<DB>
where
    DB: ArchiveDBProvider + Sync + Send + 'static,
{
    type Error = Error<<DB::StateDBReadWrite as StateDBRead>::Error>;
    type StateDB = MultiStateDB<DB>;

    fn state_at(&self, block_arg: BlockId) -> Result<Option<Self::StateDB>, Self::Error> {
        let snapshot = self
            .snapshot_tree
            .state_at(block_arg)
            .map_err(Error::Snapshot)?;
        if let Some(snapshot) = snapshot {
            return Ok(Some(MultiStateDB::Snapshot(snapshot)));
        }
        let archive = self
            .history_tree
            .state_at(block_arg)
            .map_err(Error::Archive)?;
        if let Some(archive) = archive {
            Ok(Some(MultiStateDB::Archive(archive)))
        } else {
            Ok(None)
        }
    }
}

impl<DB> EvmStorageWrite for ArchiveTree<DB>
where
    DB: ArchiveDBProvider + Sync + Send + 'static,
{
    type Error = Error<<DB::StateDBReadWrite as StateDBWrite>::Error>;
    fn update_block(
        &self,
        block_info: Block<Transaction>,
        block_diff: BlockStorageDiff,
    ) -> Result<(), Self::Error> {
        let res = self
            .snapshot_tree
            .update_block(block_info.clone(), block_diff.clone())?;
        Ok(res)
    }

    fn last_committed_block(&self) -> Result<Option<Block<Transaction>>, Self::Error> {
        let res = self.snapshot_tree.last_committed_block()?;
        Ok(res)
    }
}

impl<DB> BlockIndex for ArchiveTree<DB>
where
    DB: ArchiveDBProvider + Sync + Send + 'static,
{
    type Error = Error<<DB::StateDBReadWrite as StateDBWrite>::Error>;
    fn get_block_by_id(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Block<Transaction>>, Self::Error> {
        let res = self.snapshot_tree.get_block_by_id(block_id)?;
        if res.is_none() {
            let db = self
                .history_tree
                .0
                .db_at(BlockId::Number(BlockNumberOrTag::Latest))?;
            if let Some(db) = db {
                match block_id {
                    BlockId::Hash(hash) => {
                        return Ok(db.read_block_info(hash.block_hash)?);
                    }
                    BlockId::Number(number) => match number {
                        BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                            return Ok(db.read_block_info(db.read_latest_block_hash()?)?);
                        }
                        BlockNumberOrTag::Number(number) => {
                            return Ok(db.read_block_info(db.read_block_hash(number)?)?);
                        }
                        _ => {
                            return Ok(None);
                        }
                    },
                }
            }
        }
        Ok(res)
    }

    fn get_block_by_id_arc(
        &self,
        block_id: BlockId,
    ) -> Result<Option<Arc<Block<Transaction>>>, Self::Error> {
        let res = self.snapshot_tree.get_block_by_id_arc(block_id)?;
        if res.is_none() {
            let db = self
                .history_tree
                .0
                .db_at(BlockId::Number(BlockNumberOrTag::Latest))?;
            if let Some(db) = db {
                match block_id {
                    BlockId::Hash(hash) => {
                        return Ok(db.read_block_info(hash.block_hash)?.map(Arc::new));
                    }
                    BlockId::Number(number) => match number {
                        BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
                            return Ok(db
                                .read_block_info(db.read_latest_block_hash()?)?
                                .map(Arc::new));
                        }
                        BlockNumberOrTag::Number(number) => {
                            return Ok(db
                                .read_block_info(db.read_block_hash(number)?)?
                                .map(Arc::new));
                        }
                        _ => {
                            return Ok(None);
                        }
                    },
                }
            }
        }
        Ok(res)
    }
}

impl<DB> TransactionIndex for ArchiveTree<DB>
where
    DB: ArchiveDBProvider + Sync + Send + 'static,
{
    type Error = Error<<DB::StateDBReadWrite as StateDBWrite>::Error>;

    fn get_transaction_by_hash(&self, tx_hash: H256) -> Result<Option<Transaction>, Self::Error> {
        let res = self.snapshot_tree.get_transaction_by_hash(tx_hash)?;
        Ok(res)
    }

    fn get_transaction_by_context(
        &self,
        tx_context: &TxContext,
    ) -> Result<Option<Transaction>, Self::Error> {
        let res = self.snapshot_tree.get_transaction_by_context(tx_context)?;
        Ok(res)
    }
}
