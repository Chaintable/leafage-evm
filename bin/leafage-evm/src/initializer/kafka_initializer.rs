use crate::updater::{write_offset, KafkaS3Config};
use alloy_rlp::Decodable;
use anyhow::Result;
use aws_sdk_s3::Client;
use leafage_evm_storage::EvmStorageWrite;
use leafage_evm_types::{Block, BlockStorageDiff, KafkaBlockChangeNotification, Transaction, H256};
use rdkafka::{
    consumer::{Consumer, StreamConsumer},
    ClientConfig, Message, Offset, TopicPartitionList,
};
use tracing::info;

/// [`Initializer`] is used to initialize the storage to the genesis block
pub struct Initializer<DB> {
    s3_client: Client,
    db: DB,
    kafka_s3_cfg: KafkaS3Config,
    consumer: StreamConsumer,
}

impl<DB> Initializer<DB>
where
    DB: EvmStorageWrite + Send + Sync + 'static,
{
    pub async fn new(db: DB, kafka_s3_cfg: KafkaS3Config) -> Result<Self> {
        let s3_config = aws_config::load_from_env().await;
        let s3_client = aws_sdk_s3::Client::new(&s3_config);
        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &kafka_s3_cfg.brokers)
            .set("enable.partition.eof", "false")
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false")
            .create()?;
        let mut tpl = TopicPartitionList::with_capacity(1);
        tpl.add_partition_offset(
            &kafka_s3_cfg.topic,
            kafka_s3_cfg.partition,
            Offset::Beginning,
        )?;
        consumer.assign(&tpl)?;
        Ok(Self {
            s3_client,
            db,
            kafka_s3_cfg,
            consumer,
        })
    }

    async fn get_block_diff(&self, block_hash: H256) -> Result<BlockStorageDiff> {
        let s3_key = format!("{}/stateDiff", block_hash);
        let s3_obj = self
            .s3_client
            .get_object()
            .bucket(&self.kafka_s3_cfg.bucket_name)
            .key(&s3_key)
            .send()
            .await?;
        let mut bytes = s3_obj
            .body
            .bytes()
            .expect(&format!("Failed to get object {}", s3_key));
        let block_storage_diff = BlockStorageDiff::decode(&mut bytes)?;
        Ok(block_storage_diff)
    }

    async fn get_block_info(&self, block_hash: H256) -> Result<Block<Transaction>> {
        let s3_key = format!("{}/block", block_hash);
        let s3_obj = self
            .s3_client
            .get_object()
            .bucket(&self.kafka_s3_cfg.bucket_name)
            .key(&s3_key)
            .send()
            .await?;
        let bytes = s3_obj
            .body
            .bytes()
            .expect(&format!("Failed to get object {}", s3_key));
        let block = serde_json::from_slice(&bytes)?;
        Ok(block)
    }

    pub async fn init(&mut self) -> Result<()> {
        let first_message = self.consumer.recv().await?;
        let block_change_notification: KafkaBlockChangeNotification =
            serde_json::from_slice(first_message.payload().unwrap())?;
        let first_block_hash = block_change_notification.new_blocks[0].hash;
        let first_block_info = self.get_block_info(first_block_hash).await?;
        let first_block_diff = self.get_block_diff(first_block_hash).await?;
        self.db
            .update_block(first_block_info.clone(), first_block_diff)?;
        info!(target: "initializer", "initialized to block, num {}, hash {}", first_block_info.header.number,first_block_info.header.hash);
        write_offset(&self.kafka_s3_cfg.offset_dir, first_message.offset() + 1)?;
        Ok(())
    }
}
