use anyhow::Result;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::TraceApiClient;
use leafage_evm_storage::{account_codec, EvmStorageWrite};
use leafage_evm_types::{
    decode_state_diff, Block, BlockId, BlockInfo, BlockNumberOrTag, HeaderInfo,
};
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
        let HeaderInfo {
            inner: header,
            other,
        } = debank_output.header;
        let block_info = BlockInfo {
            inner: Block::empty(header),
            other,
        };
        let block_diff = decode_state_diff(account_codec(), debank_output.state_diff.as_ref())?;
        self.db.update_block(block_info.clone(), block_diff)?;
        info!(target: "initializer", "initialized genesis block, num {}, hash {}", block_info.header.number, block_info.header.hash);
        Ok(())
    }
}
