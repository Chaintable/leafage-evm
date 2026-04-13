use crate::utils::{
    s3_get_block_diff, s3_get_block_info, s3_get_block_info_and_diff_by_number, KafkaS3Config,
};
use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use futures::stream::StreamExt;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_storage::{EvmStorageRead, EvmStorageWrite};
use leafage_evm_types::{
    BlockInfo, BlockStorageDiff, KafkaBlockChangeNotification, KafkaBlockContext, H256,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::{sync::watch, task::JoinSet, time};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info};

#[derive(Debug, Clone)]
struct BlockContextCache {
    block_diff: BlockStorageDiff,
    block_info: BlockInfo,
}

/// [`Updater`] subscribes to an external WebSocket endpoint that pushes the
/// same [`KafkaBlockChangeNotification`] JSON payload delivered by kafka, and
/// updates the snapshot tree to the latest block. On every fresh connection
/// it first catches up from S3 (and optionally RPC) to the block number
/// referenced by the first WS message, then switches to live WS processing.
pub struct Updater<Tree> {
    ws_url: String,
    rpc_client: Option<HttpClient>,
    kafka_s3_cfg: KafkaS3Config,
    s3_client: Client,
    tree: Tree,
    max_diff_depth: usize,
    init_task_queue_size: usize,
    hash_to_blockctx: Mutex<HashMap<H256, BlockContextCache>>,
    read_from_ws: bool,
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
        ws_url: String,
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
        let s3_config = aws_config::load_from_env().await;
        let s3_client = aws_sdk_s3::Client::new(&s3_config);

        Ok(Self {
            ws_url,
            rpc_client,
            kafka_s3_cfg,
            s3_client,
            tree,
            max_diff_depth,
            init_task_queue_size,
            hash_to_blockctx: Mutex::new(HashMap::default()),
            read_from_ws: false,
        })
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

    fn clear(&self, persist_block_num: u64, persist_block_hash: H256) {
        self.hash_to_blockctx
            .lock()
            .unwrap()
            .remove(&persist_block_hash);
        self.hash_to_blockctx
            .lock()
            .unwrap()
            .retain(|_, block| block.block_info.header.number >= persist_block_num);
    }

    async fn prepare_update(
        &self,
        notifications: Vec<KafkaBlockChangeNotification>,
    ) -> Result<Vec<KafkaBlockContext>> {
        let mut new_blocks = vec![];
        let mut get_block_info_join_set = JoinSet::new();
        let mut get_block_diff_join_set = JoinSet::new();

        for notif in &notifications {
            for new_block in notif.new_blocks.iter() {
                let client = self.s3_client.clone();
                let bucket_name = self.kafka_s3_cfg.bucket_name.clone();
                let s3_chain_id = self.kafka_s3_cfg.s3_chain_id.clone();
                let version = self.kafka_s3_cfg.version.clone();
                let hash = new_block.hash;
                get_block_info_join_set.spawn(async move {
                    s3_get_block_info(&client, &bucket_name, &s3_chain_id, &version, hash).await
                });
            }
        }

        let mut blockhash_to_block_info = HashMap::new();
        let mut roothash_to_block_info = HashMap::new();

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

        while let Some(res) = get_block_diff_join_set.join_next().await {
            let block_diff = res??;
            roothash_to_block_info.insert(block_diff.hash, block_diff);
        }

        for mut notif in notifications {
            debug!(target:"updater", "get block_change_notification {:?}", notif);
            for new_block in notif.new_blocks.drain(..) {
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

                self.hash_to_blockctx.lock().unwrap().insert(
                    new_block.hash,
                    BlockContextCache {
                        block_diff,
                        block_info,
                    },
                );

                new_blocks.push(new_block);
            }
        }
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

    async fn update_from_s3(&self, notifications: &[KafkaBlockChangeNotification]) -> Result<()> {
        let target_block = notifications
            .first()
            .and_then(|n| n.new_blocks.first())
            .ok_or_else(|| anyhow::anyhow!("No new blocks in the message"))?
            .clone();
        let target_block_number = target_block.block_number - 1;
        let mut start_block_number = self.tree.last_committed_block()?.unwrap().header.number + 1;
        let batch_size = self.init_task_queue_size as u64;
        info!(target:"updater", "update from s3, start block number {}, target block number {}", start_block_number, target_block_number);
        while start_block_number <= target_block_number {
            let end_block_number =
                std::cmp::min(start_block_number + batch_size - 1, target_block_number);
            self.update_range_from_s3(start_block_number, end_block_number)
                .await?;
            start_block_number += batch_size;
        }
        Ok(())
    }

    async fn update_from_ws(
        &self,
        notifications: Vec<KafkaBlockChangeNotification>,
    ) -> Result<()> {
        let new_blocks = self.prepare_update(notifications).await?;
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
        self.prune_cache()
    }

    fn prune_cache(&self) -> Result<()> {
        let persist_block = self.tree.last_committed_block()?.unwrap();
        let persist_block_num = persist_block.header.number;
        let persist_block_hash = persist_block.header.hash;
        self.clear(persist_block_num, persist_block_hash);
        Ok(())
    }

    /// Decide whether the first batch after a fresh connection is too far ahead
    /// of the last committed block. If so, fall back to S3/RPC backfill first.
    fn needs_s3_catchup(&self, notifications: &[KafkaBlockChangeNotification]) -> Result<bool> {
        let Some(target_block) = notifications.first().and_then(|n| n.new_blocks.first()) else {
            return Ok(false);
        };
        let last_committed = self.tree.last_committed_block()?.unwrap().header.number;
        Ok(target_block.block_number > last_committed + 1)
    }

    /// Run one connection lifecycle. Returns when the peer closes or the
    /// stream errors. Each fresh call behaves as an initial connection:
    /// `read_from_ws` is reset so the first batch triggers S3 catchup if the
    /// tree is behind.
    async fn serve(&mut self) -> Result<()> {
        info!(target:"updater", "connecting to ws endpoint {}", self.ws_url);
        let (ws_stream, _resp) = connect_async(&self.ws_url).await?;
        let (_sink, read) = ws_stream.split();
        let mut chunks = read.ready_chunks(std::cmp::max(1, self.max_diff_depth));

        self.read_from_ws = false;

        while let Some(messages) = chunks.next().await {
            let mut notifications = Vec::with_capacity(messages.len());
            for message in messages {
                match message? {
                    Message::Text(text) => {
                        let notif: KafkaBlockChangeNotification = serde_json::from_str(&text)?;
                        notifications.push(notif);
                    }
                    Message::Binary(bytes) => {
                        let notif: KafkaBlockChangeNotification = bytes.as_ref().try_into()?;
                        notifications.push(notif);
                    }
                    Message::Close(frame) => {
                        info!(target:"updater", "ws closed by peer: {:?}", frame);
                        return Ok(());
                    }
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
            if notifications.is_empty() {
                continue;
            }

            if !self.read_from_ws {
                if self.needs_s3_catchup(&notifications)? {
                    loop {
                        if let Err(e) = self.update_from_s3(&notifications).await {
                            error!(target:"updater", "Failed to update from S3: {:?}", e);
                            time::sleep(Duration::from_secs(1)).await;
                        } else {
                            break;
                        }
                    }
                }
                self.read_from_ws = true;
            }

            if let Err(e) = self.update_from_ws(notifications).await {
                error!(target:"updater", "Failed to update from ws: {:?}", e);
                let _ = self.prune_cache();
                time::sleep(Duration::from_secs(1)).await;
            }
        }
        Ok(())
    }

    pub fn start(mut self) -> watch::Sender<()> {
        let (tx, mut rx) = watch::channel(());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        info!(target:"updater", "stop ws updater");
                        break;
                    }
                    res = self.serve() => {
                        match res {
                            Ok(()) => info!(target:"updater", "ws stream ended, reconnecting in 1s"),
                            Err(e) => error!(target:"updater", "ws updater error: {:?}, reconnecting in 1s", e),
                        }
                        time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
        tx
    }
}
