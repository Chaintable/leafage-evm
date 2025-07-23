use crate::utils::{
    s3_get_block_diff, s3_get_block_hash_by_number, s3_get_block_info, KafkaS3Config,
};
use anyhow::{Context, Ok, Result};
use aws_sdk_s3::Client;
use leafage_evm_storage::EvmStorageWrite;
use leafage_evm_types::{Block, BlockStorageDiff, Transaction, H256};
use tracing::info;

/// [`Initializer`] is used to initialize the storage to the genesis block
pub struct Initializer<DB> {
    s3_client: Client,
    db: DB,
    kafka_s3_cfg: KafkaS3Config,
    genesis_number: u64,
}

impl<DB> Initializer<DB>
where
    DB: EvmStorageWrite + Send + Sync + 'static,
{
    pub async fn new(db: DB, kafka_s3_cfg: KafkaS3Config, genesis_number: u64) -> Result<Self> {
        let s3_config = aws_config::load_from_env().await;
        let s3_client = aws_sdk_s3::Client::new(&s3_config);
        Ok(Self {
            s3_client,
            db,
            kafka_s3_cfg,
            genesis_number,
        })
    }

    #[inline]
    async fn get_block_diff(&self, block_root: H256) -> Result<BlockStorageDiff> {
        s3_get_block_diff(
            &self.s3_client,
            &self.kafka_s3_cfg.bucket_name,
            &self.kafka_s3_cfg.s3_chain_id,
            block_root,
        )
        .await
        .context(format!("Failed to get block diff for root {}", block_root))
    }

    #[inline]
    async fn get_block_info(&self, block_hash: H256) -> Result<Block<Transaction>> {
        s3_get_block_info(
            &self.s3_client,
            &self.kafka_s3_cfg.bucket_name,
            &self.kafka_s3_cfg.s3_chain_id,
            block_hash,
        )
        .await
        .context(format!("Failed to get block info for hash {}", block_hash))
    }

    async fn get_block_info_and_diff_by_number(
        &self,
        number: u64,
    ) -> Result<(Block<Transaction>, BlockStorageDiff)> {
        let hash = s3_get_block_hash_by_number(
            &self.s3_client,
            &self.kafka_s3_cfg.bucket_name,
            &self.kafka_s3_cfg.s3_chain_id,
            number,
        )
        .await?;
        let block_info = self.get_block_info(hash).await?;
        let block_diff = self.get_block_diff(block_info.header.state_root).await?;
        Ok((block_info, block_diff))
    }

    pub async fn init(&mut self) -> Result<()> {
        let (first_block_info, first_block_diff) = self
            .get_block_info_and_diff_by_number(self.genesis_number)
            .await?;
        self.db
            .update_block(first_block_info.clone(), first_block_diff)?;
        info!(target: "initializer", "initialized genesis block, num {}, hash {}", first_block_info.header.number,first_block_info.header.hash);
        Ok(())
    }
}
