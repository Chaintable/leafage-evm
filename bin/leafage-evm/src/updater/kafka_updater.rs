use crate::utils::{
    s3_get_block_diff, s3_get_block_info, s3_get_block_info_and_diff_by_number, KafkaS3Config,
};
use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use futures::stream::StreamExt;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_storage::{read_offset, write_offset, EvmStorageRead, EvmStorageWrite};
use leafage_evm_types::{
    Block, BlockStorageDiff, KafkaBlockChangeNotification, KafkaBlockContext, Transaction, H256,
};
use rdkafka::{
    consumer::{Consumer, StreamConsumer},
    message::BorrowedMessage,
    util::Timeout,
    ClientConfig, Message, Offset, TopicPartitionList,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::{sync::watch, task::JoinSet, time};
use tracing::{debug, error, info};

#[derive(Debug, Clone)]
struct BlockContextWithOffset {
    block_diff: BlockStorageDiff,
    block_info: Block<Transaction>,
    offset: i64,
}

/// [`Updater`] is used to update the snapshot tree to the latest block
pub struct Updater<Tree> {
    rpc_client: Option<HttpClient>,
    kafka_s3_cfg: KafkaS3Config,
    consumer: StreamConsumer,
    s3_client: Client,
    tree: Tree,
    max_diff_depth: usize,
    hash_to_blockctx: Mutex<HashMap<H256, BlockContextWithOffset>>,
    read_from_kafka: bool,
    init_task_queue_size: usize,
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
        rpc_url: Option<impl AsRef<str>>,
        kafka_s3_cfg: KafkaS3Config,
        max_diff_depth: usize,
        init_task_queue_size: usize,
    ) -> Result<Self> {
        let mut rpc_client = None;
        if let Some(rpc_url) = rpc_url {
            let client = HttpClientBuilder::default().build(rpc_url.as_ref())?;
            rpc_client = Some(client);
        }
        let consumer: StreamConsumer = ClientConfig::new()
            .set("bootstrap.servers", &kafka_s3_cfg.brokers)
            .set("enable.partition.eof", "false")
            .set("session.timeout.ms", "6000")
            .set("enable.auto.commit", "false")
            .set(
                "group.id",
                format!("leafage-evm-group-{}", kafka_s3_cfg.s3_chain_id),
            )
            .create()?;

        let s3_config = aws_config::load_from_env().await;
        let s3_client = aws_sdk_s3::Client::new(&s3_config);

        Ok(Self {
            rpc_client,
            kafka_s3_cfg,
            consumer,
            s3_client,
            tree,
            max_diff_depth,
            hash_to_blockctx: Mutex::new(HashMap::default()),
            read_from_kafka: true,
            init_task_queue_size,
        })
    }

    fn set_offset(&self, offset: i64) -> Result<()> {
        let mut tpl = TopicPartitionList::with_capacity(1);
        tpl.add_partition_offset(
            &self.kafka_s3_cfg.topic,
            self.kafka_s3_cfg.partition,
            Offset::Offset(offset),
        )?;
        self.consumer.assign(&tpl)?;
        self.consumer.seek(
            &self.kafka_s3_cfg.topic,
            self.kafka_s3_cfg.partition,
            Offset::Offset(offset),
            Timeout::Never,
        )?;
        Ok(())
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
        .context(format!("s3 get block info failed, {block_hash}"))
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

    async fn prepare_update(
        &self,
        messages: &Vec<BorrowedMessage<'_>>,
    ) -> Result<Vec<KafkaBlockContext>> {
        let mut msgs: Vec<(i64, KafkaBlockChangeNotification)> = vec![];
        let mut new_blocks = vec![];
        let mut get_block_info_join_set = JoinSet::new();
        let mut get_block_diff_join_set = JoinSet::new();

        // decode messages
        for msg in messages {
            let offset = msg.offset();
            let block_change_notification: KafkaBlockChangeNotification =
                msg.payload().unwrap().try_into()?;
            for new_block in block_change_notification.new_blocks.iter() {
                let client = self.s3_client.clone();
                let bucket_name = self.kafka_s3_cfg.bucket_name.clone();
                let s3_chain_id = self.kafka_s3_cfg.s3_chain_id.clone();
                let hash = new_block.hash;
                get_block_info_join_set.spawn(async move {
                    s3_get_block_info(&client, &bucket_name, &s3_chain_id, hash).await
                });
            }
            msgs.push((offset, block_change_notification));
        }

        let mut blockhash_to_block_info = HashMap::new();
        let mut roothash_to_block_info = HashMap::new();

        // get block info first
        while let Some(res) = get_block_info_join_set.join_next().await {
            let block_info = res??;
            let hash = block_info.header.hash;
            blockhash_to_block_info.insert(hash, block_info.clone());
            let parent_hash = block_info.header.parent_hash;
            let parent_block_info =
                if let Some(parent_block_info) = blockhash_to_block_info.get(&parent_hash) {
                    parent_block_info.clone()
                } else {
                    let parent_block_info = self.get_block_info(parent_hash).await?;
                    blockhash_to_block_info.insert(parent_hash, parent_block_info.clone());
                    parent_block_info
                };
            if parent_block_info.header.state_root != block_info.header.state_root {
                let client = self.s3_client.clone();
                let bucket_name = self.kafka_s3_cfg.bucket_name.clone();
                let s3_chain_id = self.kafka_s3_cfg.s3_chain_id.clone();
                let block_root = block_info.header.state_root;
                get_block_diff_join_set.spawn(async move {
                    s3_get_block_diff(&client, &bucket_name, &s3_chain_id, block_root).await
                });
            };
        }

        // get block diff
        while let Some(res) = get_block_diff_join_set.join_next().await {
            let block_diff = res??;
            roothash_to_block_info.insert(block_diff.hash, block_diff);
        }

        for (offset, mut block_change_notification) in msgs.drain(..) {
            debug!(target:"updater", "get block_change_notification {:?}, offset {:?}", block_change_notification, offset);
            for new_block in block_change_notification.new_blocks.drain(..) {
                let parent_block_info = &blockhash_to_block_info[&new_block.parent_hash];
                let block_info = blockhash_to_block_info[&new_block.hash].clone();

                let block_diff =
                    if parent_block_info.header.state_root == block_info.header.state_root {
                        let mut diff = BlockStorageDiff::default();
                        diff.hash = block_info.header.state_root;
                        diff.parent_hash = parent_block_info.header.state_root;
                        diff
                    } else {
                        roothash_to_block_info[&block_info.header.state_root].clone()
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

                new_blocks.push(new_block);
            }
        }
        Ok(new_blocks)
    }

    async fn update_from_s3(&self, messages: &Vec<BorrowedMessage<'_>>) -> Result<()> {
        let block_change_notification: KafkaBlockChangeNotification =
            messages[0].payload().unwrap().try_into()?;
        let target_block = block_change_notification
            .new_blocks
            .first()
            .ok_or_else(|| anyhow::anyhow!("No new blocks in the message"))?
            .clone();
        let target_block_number = target_block.block_number - 1;
        let mut start_block_number = self.tree.last_committed_block()?.unwrap().header.number + 1;
        info!(target:"updater", "update from s3, start block number {}, target block number {}", start_block_number, target_block_number);
        while start_block_number <= target_block_number {
            let mut get_block_info_diff_join_set = JoinSet::new();
            let batch_size = self.init_task_queue_size as u64;
            let end_block_number =
                std::cmp::min(start_block_number + batch_size - 1, target_block_number);
            for block_number in start_block_number..=end_block_number {
                let rpc_client = self.rpc_client.clone();
                let client = self.s3_client.clone();
                let bucket_name = self.kafka_s3_cfg.bucket_name.clone();
                let outer_bucket_name = self.kafka_s3_cfg.outer_bucket_name.clone();
                let s3_chain_id = self.kafka_s3_cfg.s3_chain_id.clone();
                get_block_info_diff_join_set.spawn(async move {
                    (
                        block_number,
                        s3_get_block_info_and_diff_by_number(
                            &rpc_client,
                            &client,
                            &bucket_name,
                            &outer_bucket_name,
                            &s3_chain_id,
                            block_number,
                        )
                        .await,
                    )
                });
            }
            let mut all_results = get_block_info_diff_join_set.join_all().await;
            all_results.sort_by_key(|(i, _)| *i);
            for (_, res) in all_results {
                match res {
                    Ok((block_info, block_diff)) => {
                        info!(target:"updater", "update block number {}, hash {}, parent hash {}", block_info.header.number, block_info.header.hash, block_info.header.parent_hash);
                        self.tree.update_block(block_info.clone(), block_diff)?;
                    }
                    Err(e) => {
                        tracing::error!(target: "etl", "Join error: {}", e);
                        return Err(anyhow::anyhow!("Failed to join tasks: {}", e));
                    }
                }
            }
            info!(target:"updater", "update from s3, start block number {}, end block number {}", start_block_number, end_block_number);

            start_block_number += batch_size;
        }

        Ok(())
    }

    async fn update_from_kafka(&self, messages: &Vec<BorrowedMessage<'_>>) -> Result<()> {
        let new_blocks = self.prepare_update(messages).await?;
        let mut update_path = new_blocks
            .iter()
            .map(|new_block| {
                self.hash_to_blockctx
                    .lock()
                    .unwrap()
                    .get(&new_block.hash)
                    .cloned()
                    .unwrap()
            })
            .collect::<Vec<_>>();
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
        self.commit_offset()
    }

    fn commit_offset(&self) -> Result<()> {
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

    async fn get_latest_offset(&self) -> Result<i64> {
        let (low, high) = self.consumer.fetch_watermarks(
            &self.kafka_s3_cfg.topic,
            self.kafka_s3_cfg.partition,
            Duration::from_secs(1),
        )?;
        if low == high {
            return Err(anyhow::anyhow!("No messages in the topic"));
        }
        return Ok(high - 1);
    }

    pub fn start(mut self) -> watch::Sender<()> {
        let (tx, mut rx) = watch::channel(());
        tokio::spawn(async move {
            let offset = read_offset(&self.kafka_s3_cfg.offset_dir).ok();
            if let Some(offset) = offset {
                self.set_offset(offset).expect("Failed to set offset");
                info!(target: "updater", "kafka updater start with offset {}", offset);
            } else {
                info!(target: "updater", "kafka updater start with no offset, will read from s3");
                self.read_from_kafka = false;
                let latest_offset = self
                    .get_latest_offset()
                    .await
                    .expect("Failed to get latest offset");
                self.set_offset(latest_offset)
                    .expect("Failed to set latest offset");
            }
            let stream = self.consumer.stream();
            let mut chunk = stream.ready_chunks(std::cmp::max(1, self.max_diff_depth));
            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        info!(target:"updater", "stop updater");
                        break;
                    }
                    messages = chunk.next() => {
                        let messages = messages.expect("kafka stream next failed");
                        let mut msgs = vec![];
                        for message in messages {
                           if message.is_err() {
                                error!(target:"updater", "Failed to receive message: {:?}", message.err());
                                break;
                            }
                            msgs.push(message.unwrap());
                        }
                        if msgs.is_empty() {
                            continue
                        }
                        if !self.read_from_kafka {
                            loop {
                                if let Err(e) = self.update_from_s3(&msgs).await {
                                    error!(target:"updater", "Failed to update from S3: {:?}", e);
                                    time::sleep(time::Duration::from_secs(1)).await
                                } else {
                                    self.read_from_kafka = true;
                                    break;
                                }
                            }
                        }
                        if self.read_from_kafka {
                            loop {
                                if let Err(e) = self.update_from_kafka(&msgs).await {
                                    error!(target:"updater", "Failed to update: {:?}", e);
                                    self.commit_offset().expect("Failed to commit offset");
                                    time::sleep(time::Duration::from_secs(1)).await
                                } else {
                                    break;
                                }
                            }
                        }

                    }
                }
            }
        });

        tx
    }
}
