use alloy_rlp::Decodable;
use anyhow::Result;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::TraceApiClient;
use leafage_evm_storage::EvmStorageWrite;
use leafage_evm_types::{Block, BlockId, BlockNumberOrTag, BlockStorageDiff};
use tracing::info;

/// [`Initializer`] is used to initialize the storage to the genesis block
pub struct Initializer<DB> {
    rpc_client: HttpClient,
    db: DB,
}

impl<DB> Initializer<DB>
where
    DB: EvmStorageWrite + Send + Sync + 'static,
{
    pub fn new(db: DB, rpc_url: impl AsRef<str>) -> Result<Self> {
        let rpc_client = HttpClientBuilder::default().build(rpc_url)?;
        Ok(Self { rpc_client, db })
    }

    pub async fn init(&mut self) -> Result<()> {
        let debank_output = self
            .rpc_client
            .debank_block(BlockId::Number(BlockNumberOrTag::Number(0)))
            .await?;
        let block_info = Block::empty(debank_output.header.clone());
        let block_diff: BlockStorageDiff =
            BlockStorageDiff::decode(&mut debank_output.state_diff.as_ref())?;
        self.db.update_block(block_info.clone(), block_diff)?;
        info!(target: "initializer", "initialized genesis block, num {}, hash {}", block_info.header.number, block_info.header.hash);
        Ok(())
    }
}
