use alloy_rlp::Decodable;
use anyhow::Result;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::{EthApiClient, TraceApiClient};
use leafage_evm_storage::EvmStorageWrite;
use leafage_evm_types::{Block, BlockId, BlockNumberOrTag, BlockStorageDiff, Transaction};
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
        let latest_block = self
            .rpc_client
            .get_block_by_number(BlockNumberOrTag::Number(0), true)
            .await?;
        if latest_block.is_none() {
            return Err(anyhow::anyhow!("failed to get genesis block"));
        }
        let latest_block = latest_block.unwrap();
        let latest_block_info: Block<Transaction> = serde_json::from_value(latest_block)?;
        let laest_block_diff = self
            .rpc_client
            .block_state_diff(BlockId::Hash(latest_block_info.header.hash.into()), false)
            .await?;
        let laest_block_diff: BlockStorageDiff =
            BlockStorageDiff::decode(&mut laest_block_diff.as_ref())?;
        self.db
            .update_block(latest_block_info.clone(), laest_block_diff)?;
        info!(target: "initializer", "initialized genesis block, num {}, hash {}", latest_block_info.header.number,latest_block_info.header.hash);
        Ok(())
    }
}
