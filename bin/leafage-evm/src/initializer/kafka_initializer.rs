use crate::utils::{s3_get_block_diff, s3_get_block_info, KafkaS3Config};
use anyhow::{Context, Ok, Result};
use aws_sdk_s3::Client;
use leafage_evm_storage::{write_offset, EvmStorageWrite};
use leafage_evm_types::{Block, BlockStorageDiff, KafkaBlockChangeNotification, Transaction, H256};
use rdkafka::{
    consumer::{Consumer, StreamConsumer},
    util::Timeout,
    ClientConfig, Message, Offset, TopicPartitionList,
};
use tracing::{debug, info};

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
            .set("group.id", H256::random().to_string())
            .create()?;
        let meta = consumer.fetch_metadata(Some(&kafka_s3_cfg.topic), Timeout::Never)?;
        let mut tpl = TopicPartitionList::with_capacity(1);
        for topic in meta.topics() {
            if topic.name() == kafka_s3_cfg.topic {
                for p in topic.partitions() {
                    tpl.add_partition_offset(&kafka_s3_cfg.topic, p.id(), Offset::Beginning)?;
                }
            }
        }
        consumer.assign(&tpl)?;
        Ok(Self {
            s3_client,
            db,
            kafka_s3_cfg,
            consumer,
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

    pub async fn init(&mut self) -> Result<()> {
        let first_message = self.consumer.recv().await?;
        let block_change_notification: KafkaBlockChangeNotification =
            first_message.payload().unwrap().try_into()?;
        let first_block_hash = block_change_notification.new_blocks[0].hash;
        let first_block_info = self.get_block_info(first_block_hash).await?;
        debug!(
            target: "initializer",
            "first block info: number {}, hash {}, parent_hash {}",
            first_block_info.header.number,
            first_block_info.header.hash,
            first_block_info.header.parent_hash
        );
        let first_block_diff = self
            .get_block_diff(first_block_info.header.state_root)
            .await?;
        self.db
            .update_block(first_block_info.clone(), first_block_diff)?;
        info!(target: "initializer", "initialized genesis block, num {}, hash {}", first_block_info.header.number,first_block_info.header.hash);
        write_offset(&self.kafka_s3_cfg.offset_dir, first_message.offset() + 1)?;
        Ok(())
    }
}
