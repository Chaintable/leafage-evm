use crate::utils::{s3_get_block_diff, s3_get_block_info, KafkaS3Config};
use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use leafage_evm_storage::{EvmStorageRead, EvmStorageWrite};
use leafage_evm_types::{
    Block, BlockId, BlockStorageDiff, KafkaBlockChangeNotification, Transaction, H256,
};
use rdkafka::{
    consumer::{Consumer, StreamConsumer},
    message::BorrowedMessage,
    util::Timeout,
    ClientConfig, Message, Offset, TopicPartitionList,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tokio::{sync::watch, time};
use tracing::{debug, error, info};

pub fn read_offset(offset_dir: &str) -> Result<i64> {
    let offset = std::fs::read_to_string(format!("{}/offset", offset_dir))?;
    let offset = offset.trim().parse()?;
    Ok(offset)
}

pub fn write_offset(offset_dir: &str, offset: i64) -> Result<()> {
    std::fs::create_dir_all(offset_dir)?;
    std::fs::write(format!("{}/offset.tmp", offset_dir), offset.to_string())?;
    std::fs::rename(
        format!("{}/offset.tmp", offset_dir),
        format!("{}/offset", offset_dir),
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
struct BlockContextWithOffset {
    block_diff: BlockStorageDiff,
    block_info: Block<Transaction>,
    offset: i64,
}

/// [`Updater`] is used to update the snapshot tree to the latest block
pub struct Updater<Tree> {
    kafka_s3_cfg: KafkaS3Config,
    consumer: StreamConsumer,
    s3_client: Client,
    tree: Tree,
    max_diff_depth: usize,
    hash_to_blockctx: Mutex<HashMap<H256, BlockContextWithOffset>>,
}

impl<Tree> Updater<Tree>
where
    Tree: EvmStorageRead
        + EvmStorageWrite<Error = <Tree as EvmStorageRead>::Error>
        + Send
        + Sync
        + 'static,
{
    pub async fn new(
        tree: Tree,
        kafka_s3_cfg: KafkaS3Config,
        max_diff_depth: usize,
    ) -> Result<Self> {
        let offset = read_offset(&kafka_s3_cfg.offset_dir)?;
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
                    if p.id() != kafka_s3_cfg.partition {
                        tpl.add_partition_offset(&kafka_s3_cfg.topic, p.id(), Offset::Beginning)?;
                    } else {
                        tpl.add_partition_offset(
                            &kafka_s3_cfg.topic,
                            p.id(),
                            Offset::Offset(offset),
                        )?;
                    }
                }
            }
        }
        consumer.assign(&tpl)?;
        consumer.seek(
            &kafka_s3_cfg.topic,
            kafka_s3_cfg.partition,
            Offset::Offset(offset),
            Timeout::Never,
        )?;

        let s3_config = aws_config::load_from_env().await;
        let s3_client = aws_sdk_s3::Client::new(&s3_config);

        Ok(Self {
            kafka_s3_cfg,
            consumer,
            s3_client,
            tree,
            max_diff_depth,
            hash_to_blockctx: Mutex::new(HashMap::default()),
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
        .context("s3 get block diff failed")
    }

    #[inline]
    async fn get_block_info(&self, block_hash: H256) -> Result<Block<Transaction>> {
        if let Some(block_ctx) = self.hash_to_blockctx.lock().unwrap().get(&block_hash) {
            return Ok(block_ctx.block_info.clone());
        }
        s3_get_block_info(
            &self.s3_client,
            &self.kafka_s3_cfg.bucket_name,
            &self.kafka_s3_cfg.s3_chain_id,
            block_hash,
        )
        .await
        .context("s3 get block info failed")
    }

    fn get_update_path(
        &self,
        latest_remote_block: BlockContextWithOffset,
    ) -> VecDeque<BlockContextWithOffset> {
        let mut update_path = VecDeque::new();
        update_path.push_back(latest_remote_block);
        loop {
            if update_path.len() > self.max_diff_depth {
                error!(target:"updater", "can't find parent block before max diff depth, drop");
                return Default::default();
            }
            let first_block_info = update_path.front().unwrap();
            if self
                .tree
                .state_at(BlockId::Hash(
                    first_block_info.block_info.header.parent_hash.into(),
                ))
                .is_ok()
            {
                debug!(target:"updater", "find parent block {}", first_block_info.block_info.header.parent_hash);
                break;
            }
            let parent_block_info = self
                .hash_to_blockctx
                .lock()
                .unwrap()
                .get(&first_block_info.block_info.header.parent_hash)
                .cloned();
            if parent_block_info.is_none() {
                error!(target:"updater", "can't not find block {}", first_block_info.block_info.header.parent_hash);
                return Default::default();
            } else {
                update_path.push_front(parent_block_info.unwrap().clone());
            }
        }
        update_path
    }

    fn clear(
        &self,
        presist_block_num: u64,
        presist_block_hash: H256,
    ) -> Option<BlockContextWithOffset> {
        let presist_block = self
            .hash_to_blockctx
            .lock()
            .unwrap()
            .remove(&presist_block_hash);

        self.hash_to_blockctx
            .lock()
            .unwrap()
            .retain(|_, block| block.block_info.header.number >= presist_block_num);

        presist_block
    }

    async fn update(&self, message: &BorrowedMessage<'_>) -> Result<()> {
        let offset = message.offset();
        let block_change_notification: KafkaBlockChangeNotification =
            message.payload().unwrap().try_into()?;

        debug!(target:"updater", "get block_change_notification {:?}, offset {:?}", block_change_notification, offset);
        for new_block in block_change_notification.new_blocks.iter() {
            let parent_block_info = self.get_block_info(new_block.parent_hash).await?;
            let block_info = self.get_block_info(new_block.hash).await?;

            let block_diff = if parent_block_info.header.state_root == block_info.header.state_root
            {
                let mut diff = BlockStorageDiff::default();
                diff.hash = block_info.header.state_root;
                diff.parent_hash = parent_block_info.header.state_root;
                diff
            } else {
                self.get_block_diff(block_info.header.state_root).await?
            };

            let block_ctx_with_offset = BlockContextWithOffset {
                block_diff,
                block_info,
                offset,
            };

            self.hash_to_blockctx
                .lock()
                .unwrap()
                .insert(new_block.hash, block_ctx_with_offset);
        }

        let latest_remote_block_ctx = block_change_notification
            .new_blocks
            .last()
            .expect("Empty new block change notification");
        let latest_remote_block = self
            .hash_to_blockctx
            .lock()
            .unwrap()
            .get(&latest_remote_block_ctx.hash)
            .expect("Empty latest remote block")
            .clone();
        let mut update_path = self.get_update_path(latest_remote_block);
        for block in update_path.drain(..) {
            let block_storage_diff = block.block_diff;
            let block_info = block.block_info;
            let block_hash = block_info.header.hash;
            let block_num = block_info.header.number;
            let new_accounts_num = block_storage_diff.new_accounts.len();
            let deleted_accounts_num = block_storage_diff.deleted_accounts.len();
            let new_codes_num = block_storage_diff.new_codes.len();
            self.tree.update_block(block_info, block_storage_diff)?;
            info!(target:"updater", "update block hash {}, block num {}, new accounts num {}, deleted accounts num {}, new codes num {}", 
                                        block_hash, block_num, new_accounts_num, deleted_accounts_num, new_codes_num);
        }
        let presist_block = self.tree.last_committed_block()?.unwrap();
        let presist_block_num = presist_block.header.number;
        let presist_block_hash = presist_block.header.hash;
        // clear block context before presist block
        let presist_block = self.clear(presist_block_num, presist_block_hash);
        if let Some(presist_block) = presist_block {
            debug!(target:"updater", "clear block hash {}, block num {}", presist_block.block_info.header.hash, presist_block.block_info.header.number);
            write_offset(&self.kafka_s3_cfg.offset_dir, presist_block.offset + 1)?;
        }
        Ok(())
    }

    pub fn start(self) -> watch::Sender<()> {
        let (tx, mut rx) = watch::channel(());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        info!(target:"updater", "stop updater");
                        break;
                    }
                    message = self.consumer.recv() => {
                        match message {
                            Ok(msg) => {
                                loop {
                                    if let Err(e) = self.update(&msg).await {
                                        error!(target:"updater", "Failed to update: {:?}", e);
                                        time::sleep(time::Duration::from_secs(1)).await
                                    } else {
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                error!(target:"updater", "Failed to receive message: {:?}", e);
                            }
                        }
                    }
                }
            }
        });

        tx
    }
}
