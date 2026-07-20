use crate::utils::{
    s3_get_block_diff, s3_get_block_info, s3_get_block_info_and_diff_by_hash,
    s3_get_block_info_and_diff_by_number, KafkaS3Config,
};
use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use futures::stream::StreamExt;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_storage::{
    read_offset, write_offset, BlockContext, EvmStorageRead, EvmStorageWrite,
};
use leafage_evm_types::{
    BlockId, BlockInfo, BlockNumberOrTag, BlockStorageDiff, KafkaBlockChangeNotification,
    KafkaBlockContext, H256,
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
    block_info: BlockInfo,
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
    /// Reorg buffer depth for S3 catch-up: the number of blocks below the
    /// Kafka head that are backfilled by following the exact parent-hash chain
    /// instead of the by-number index. 0 disables it (legacy behavior).
    catchup_safe_depth: usize,
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
        catchup_safe_depth: usize,
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
            catchup_safe_depth,
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
    async fn get_block_info(&self, block_hash: H256) -> Result<BlockInfo> {
        if let Some(block_ctx) = self.hash_to_blockctx.lock().unwrap().get(&block_hash) {
            return Ok(block_ctx.block_info.clone());
        }
        s3_get_block_info(
            &self.s3_client,
            &self.kafka_s3_cfg.bucket_name,
            &self.kafka_s3_cfg.s3_chain_id,
            &self.kafka_s3_cfg.version,
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
        let mut blocks = self.hash_to_blockctx.lock().unwrap();
        let presist_block = blocks.remove(&presist_block_hash);
        blocks.retain(|_, block| block.block_info.header.number >= presist_block_num);

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
                let version = self.kafka_s3_cfg.version.clone();
                let hash = new_block.hash;
                get_block_info_join_set.spawn(async move {
                    s3_get_block_info(&client, &bucket_name, &s3_chain_id, &version, hash).await
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
                let version = self.kafka_s3_cfg.version.clone();
                let block_root = block_info.header.state_root;
                get_block_diff_join_set.spawn(async move {
                    s3_get_block_diff(&client, &bucket_name, &s3_chain_id, &version, block_root)
                        .await
                });
            };
        }

        // get block diff
        while let Some(res) = get_block_diff_join_set.join_next().await {
            let block_diff = res??;
            roothash_to_block_info.insert(block_diff.hash, block_diff);
        }

        let mut block_contexts = Vec::new();
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

                block_contexts.push((new_block.hash, block_ctx_with_offset));
                new_blocks.push(new_block);
            }
        }
        self.hash_to_blockctx.lock().unwrap().extend(block_contexts);
        Ok(new_blocks)
    }

    async fn update_range_from_s3(
        &self,
        start_block_number: u64,
        end_block_number: u64,
    ) -> Result<()> {
        let mut get_block_info_diff_join_set = JoinSet::new();
        for block_number in start_block_number..=end_block_number {
            let rpc_client = self.rpc_client.clone();
            let client = self.s3_client.clone();
            let bucket_name = self.kafka_s3_cfg.bucket_name.clone();
            let outer_bucket_name = self.kafka_s3_cfg.outer_bucket_name.clone();
            let s3_chain_id = self.kafka_s3_cfg.s3_chain_id.clone();
            let version = self.kafka_s3_cfg.version.clone();
            get_block_info_diff_join_set.spawn(async move {
                (
                    block_number,
                    s3_get_block_info_and_diff_by_number(
                        &rpc_client,
                        &client,
                        &bucket_name,
                        &outer_bucket_name,
                        &s3_chain_id,
                        &version,
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
                    error!(target: "etl", "Join error: {}", e);
                    return Err(anyhow::anyhow!("Failed to join tasks: {}", e));
                }
            }
        }
        info!(target:"updater", "update from s3, start block number {}, end block number {}", start_block_number, end_block_number);
        Ok(())
    }

    async fn update_from_s3(&self, messages: &Vec<BorrowedMessage<'_>>) -> Result<()> {
        let block_change_notification: KafkaBlockChangeNotification =
            messages[0].payload().unwrap().try_into()?;
        let target_block = block_change_notification
            .new_blocks
            .first()
            .ok_or_else(|| anyhow::anyhow!("No new blocks in the message"))?
            .clone();
        let last_committed_number = self.tree.last_committed_block()?.unwrap().header.number;
        let tip_block_number = target_block.block_number.saturating_sub(1);

        // The by-number S3 index can resolve the wrong branch around the chain
        // tip during a reorg, which leaves the hand-off block disconnected from
        // the Kafka stream. So only trust by-number for the stable segment and
        // leave a `catchup_safe_depth` buffer below the Kafka head; that buffer
        // must exceed the chain's maximum reorg depth so the by-number hand-off
        // block is always canonical. The buffered tip is then backfilled along
        // the exact parent-hash links from Kafka (phase 2 below). A depth of 0
        // disables the buffer entirely, falling back to the legacy
        // by-number-only catch-up.
        let depth = self.catchup_safe_depth as u64;
        // Backfill the `depth` blocks immediately below the Kafka head (the tip
        // and the `depth - 1` blocks beneath it), so the by-number hand-off
        // block sits `depth` blocks below the tip — outside a reorg of depth
        // `<= depth`. Basing this on `tip` (not the Kafka head) keeps the flag
        // honest: `depth = 1` protects exactly the tip, `depth = 0` protects
        // nothing (legacy by-number-only catch-up).
        let by_number_target = tip_block_number
            .saturating_sub(depth)
            .max(last_committed_number)
            .min(tip_block_number);

        // Phase 1: by-number catch-up over the stable segment.
        let batch_size = self.init_task_queue_size as u64;
        let mut start_block_number = last_committed_number + 1;
        info!(target:"updater", "update from s3 by number, start block number {}, target block number {}", start_block_number, by_number_target);
        while start_block_number <= by_number_target {
            let end_block_number =
                std::cmp::min(start_block_number + batch_size - 1, by_number_target);
            self.update_range_from_s3(start_block_number, end_block_number)
                .await?;
            start_block_number += batch_size;
        }

        // Phase 2: backfill (by_number_target, tip] by walking the parent-hash
        // chain from the Kafka head, reading each block strictly by hash so a
        // tip reorg cannot swap in a sibling from the wrong branch.
        let mut backfill = Vec::new();
        if tip_block_number > by_number_target {
            let mut parent_hash = target_block.parent_hash;
            // A healthy parent-hash chain decrements the block number by one per
            // hop, so it reaches `by_number_target` within exactly this many
            // blocks. The bound guards against an unbounded walk (and S3 request
            // storm) should the chain data be corrupt or non-decreasing.
            let max_hops = tip_block_number - by_number_target;
            loop {
                let (block_info, block_diff) = s3_get_block_info_and_diff_by_hash(
                    &self.s3_client,
                    &self.kafka_s3_cfg.bucket_name,
                    &self.kafka_s3_cfg.s3_chain_id,
                    &self.kafka_s3_cfg.version,
                    parent_hash,
                )
                .await?;
                if block_info.header.number <= by_number_target {
                    break;
                }
                parent_hash = block_info.header.parent_hash;
                backfill.push((block_info, block_diff));
                if backfill.len() as u64 >= max_hops {
                    break;
                }
            }
        }
        // A reorg deeper than the buffer would leave the by-number hand-off
        // block on a stale branch: the oldest backfilled block then links to a
        // parent that isn't in the tree, and update_block would fail with an
        // opaque ParentBlockHashNotFound. Detect it here and report the real
        // cause (and the fix) instead. The chain anchor is the oldest block's
        // parent_hash, so no extra S3 read is needed.
        if let Some((oldest, _)) = backfill.last() {
            let tree_anchor_hash = self
                .tree
                .state_at(BlockId::Number(BlockNumberOrTag::Number(by_number_target)))?
                .map(|s| s.block_info())
                .transpose()?
                .map(|b| b.header.hash);
            if tree_anchor_hash != Some(oldest.header.parent_hash) {
                return Err(anyhow::anyhow!(
                    "S3 catch-up hand-off mismatch at block {}: by-number anchor {:?} != Kafka chain parent {}; \
                     reorg is deeper than --catchup-safe-depth ({}), increase it",
                    by_number_target,
                    tree_anchor_hash,
                    oldest.header.parent_hash,
                    depth
                ));
            }
        }
        for (block_info, block_diff) in backfill.into_iter().rev() {
            info!(target:"updater", "update from s3 by hash, block number {}, hash {}, parent hash {}", block_info.header.number, block_info.header.hash, block_info.header.parent_hash);
            self.tree.update_block(block_info, block_diff)?;
        }
        Ok(())
    }

    async fn update_from_kafka(&self, messages: &Vec<BorrowedMessage<'_>>) -> Result<()> {
        let new_blocks = self.prepare_update(messages).await?;
        let mut update_path = {
            let blocks = self.hash_to_blockctx.lock().unwrap();
            new_blocks
                .iter()
                .map(|new_block| blocks.get(&new_block.hash).cloned().unwrap())
                .collect::<Vec<_>>()
        };
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

    async fn get_offset(&self) -> Result<(i64, i64)> {
        let (low, high) = self.consumer.fetch_watermarks(
            &self.kafka_s3_cfg.topic,
            self.kafka_s3_cfg.partition,
            Duration::from_secs(1),
        )?;
        if low == high {
            return Err(anyhow::anyhow!("No messages in the topic"));
        }
        return Ok((low, high - 1));
    }

    async fn init_offset(&mut self) {
        let offset = read_offset(&self.kafka_s3_cfg.offset_dir).ok();
        let (lowest_offset, latest_offset) = self
            .get_offset()
            .await
            .expect("Failed to get latest offset");
        match offset {
            Some(offset) if offset >= lowest_offset => {
                self.set_offset(offset).expect("Failed to set offset");
                info!(target: "updater", "kafka updater start with offset {}", offset);
            }
            Some(offset) => {
                info!(target: "updater", "offset {} is smaller than lowest offset {}, will read from s3/rpc", offset, lowest_offset);
                self.read_from_kafka = false;
                self.set_offset(latest_offset)
                    .expect("Failed to set latest offset");
            }
            None => {
                info!(target: "updater", "kafka updater start with no offset, will read from s3/rpc");
                self.read_from_kafka = false;
                self.set_offset(latest_offset)
                    .expect("Failed to set latest offset");
            }
        }
    }

    pub fn start(mut self) -> watch::Sender<()> {
        let (tx, mut rx) = watch::channel(());
        tokio::spawn(async move {
            self.init_offset().await;
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
