use crate::utils::{s3_get_block_info_and_diff_by_number_for_genesis, KafkaS3Config};
use anyhow::{Ok, Result};
use aws_sdk_s3::Client;
use leafage_evm_storage::EvmStorageWrite;
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

    pub async fn init(&mut self) -> Result<()> {
        let (first_block_info, first_block_diff) =
            s3_get_block_info_and_diff_by_number_for_genesis(
                &self.s3_client,
                &self.kafka_s3_cfg.bucket_name,
                &self.kafka_s3_cfg.outer_bucket_name,
                &self.kafka_s3_cfg.s3_chain_id,
                self.genesis_number,
            )
            .await?;
        self.db
            .update_block(first_block_info.clone(), first_block_diff)?;
        info!(target: "initializer", "initialized genesis block, num {}, hash {}", first_block_info.header.number,first_block_info.header.hash);
        Ok(())
    }
}
