use alloy_rlp::Decodable;
use crate::utils::{
    s3_get_block_diff, s3_get_block_info, s3_get_block_info_and_diff_by_number, GatewayObjectConfig, KafkaS3Config,
};
use anyhow::{Context, Result};
use aws_sdk_s3::Client;
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


enum ControlAction {
    Ignore,
    Continue,
    Catchup { from_block: u64, to_block: u64 },
    SnapshotRequired { manifest_url: String, reason: String, resume_from: u64 },
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct LeafageHelloRequest {
    r#type: String,
    protocol_version: String,
    chain_id: String,
    node_id: String,
    resume_cursor: LeafageResumeCursor,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct LeafageResumeCursor {
    last_seq: u64,
    last_block_number: u64,
    last_block_hash: String,
    data_version: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct LeafageHelloAck {
    r#type: String,
    protocol_version: String,
    chain_id: String,
    current_data_version: String,
    latest_height: u64,
    next_action: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct LeafageCatchupRequired {
    r#type: String,
    chain_id: String,
    current_data_version: String,
    from_block: u64,
    to_block: u64,
    reason: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct LeafageSnapshotRef {
    manifest_url: String,
    resume_from: u64,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct LeafageSnapshotRequired {
    r#type: String,
    chain_id: String,
    current_data_version: String,
    reason: String,
    snapshot: LeafageSnapshotRef,
}

/// [`Updater`] subscribes to an external WebSocket endpoint that pushes the
/// same [`KafkaBlockChangeNotification`] JSON payload delivered by kafka, and
/// updates the snapshot tree to the latest block. On every fresh connection
/// it first catches up from S3 (and optionally RPC) to the block number
/// referenced by the first WS message, then switches to live WS processing.
pub struct Updater<Tree> {
    ws_url: String,
    rpc_client: Option<HttpClient>,
    kafka_s3_cfg: Option<KafkaS3Config>,
    gateway_object_cfg: Option<GatewayObjectConfig>,
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
        kafka_s3_cfg: Option<KafkaS3Config>,
        gateway_object_cfg: Option<GatewayObjectConfig>,
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
            gateway_object_cfg,
            s3_client,
            tree,
            max_diff_depth,
            init_task_queue_size,
            hash_to_blockctx: Mutex::new(HashMap::default()),
            read_from_ws: false,
        })
    }


    fn chain_id(&self) -> &str {
        if let Some(cfg) = &self.gateway_object_cfg {
            &cfg.chain_id
        } else if let Some(cfg) = &self.kafka_s3_cfg {
            &cfg.s3_chain_id
        } else {
            ""
        }
    }

    fn data_version(&self) -> &str {
        if let Some(cfg) = &self.gateway_object_cfg {
            &cfg.version
        } else if let Some(cfg) = &self.kafka_s3_cfg {
            &cfg.version
        } else {
            ""
        }
    }

    fn build_hello_request(&self) -> Result<LeafageHelloRequest> {
        let (last_seq, last_block_number, last_block_hash) = match self.tree.last_committed_block()? {
            Some(block) => (0, block.header.number, format!("{:?}", block.header.hash)),
            None => (0, 0, String::new()),
        };
        Ok(LeafageHelloRequest {
            r#type: "hello".to_string(),
            protocol_version: "leafage-ws-v1".to_string(),
            chain_id: self.chain_id().to_string(),
            node_id: std::env::var("HOSTNAME").unwrap_or_else(|_| "leafage-evm".to_string()),
            resume_cursor: LeafageResumeCursor {
                last_seq,
                last_block_number,
                last_block_hash,
                data_version: self.data_version().to_string(),
            },
        })
    }

    async fn handle_control_message(&self, msg: &Message) -> Result<ControlAction> {
        match msg {
            Message::Text(text) => {
                let value: serde_json::Value = serde_json::from_str(text)?;
                let Some(msg_type) = value.get("type").and_then(|v| v.as_str()) else {
                    return Ok(ControlAction::Ignore);
                };
                match msg_type {
                    "hello_ack" => {
                        let ack: LeafageHelloAck = serde_json::from_value(value)?;
                        info!(target:"updater", "leafage ws hello_ack chain={} version={} latest_height={} next_action={}", ack.chain_id, ack.current_data_version, ack.latest_height, ack.next_action);
                        match ack.next_action.as_str() {
                            "replay" => Ok(ControlAction::Continue),
                            "catchup" => Ok(ControlAction::Continue),
                            "snapshot" => Ok(ControlAction::SnapshotRequired {
                                manifest_url: String::new(),
                                reason: "snapshot_required_from_hello_ack".to_string(),
                                resume_from: ack.latest_height,
                            }),
                            _ => Ok(ControlAction::Continue),
                        }
                    }
                    "catchup_required" => {
                        let control: LeafageCatchupRequired = serde_json::from_value(value)?;
                        info!(target:"updater", "leafage ws catchup_required chain={} version={} from_block={} to_block={} reason={}", control.chain_id, control.current_data_version, control.from_block, control.to_block, control.reason);
                        Ok(ControlAction::Catchup {
                            from_block: control.from_block,
                            to_block: control.to_block,
                        })
                    }
                    "snapshot_required" => {
                        let control: LeafageSnapshotRequired = serde_json::from_value(value)?;
                        info!(target:"updater", "leafage ws snapshot_required chain={} version={} reason={} manifest={} resume_from={}", control.chain_id, control.current_data_version, control.reason, control.snapshot.manifest_url, control.snapshot.resume_from);
                        Ok(ControlAction::SnapshotRequired {
                            manifest_url: control.snapshot.manifest_url,
                            reason: control.reason,
                            resume_from: control.snapshot.resume_from,
                        })
                    }
                    _ => Ok(ControlAction::Ignore),
                }
            }
            _ => Ok(ControlAction::Ignore),
        }
    }

    async fn get_gateway_object(&self, kind: &str, block_number: u64, object_key: &str) -> Result<Vec<u8>> {
        let cfg = self
            .gateway_object_cfg
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gateway object mode disabled"))?;
        let base = cfg.base_url.trim_end_matches('/');
        let url = format!(
            "{}/v1/object?chain_id={}&ref_id={}&kind={}&block_number={}",
            base,
            cfg.chain_id,
            object_key,
            kind,
            block_number
        );
        let resp = reqwest::get(&url)
            .await
            .context(format!("gateway fetch failed: {}", url))?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("gateway fetch failed status {} for {}", resp.status(), url));
        }
        Ok(resp.bytes().await?.to_vec())
    }

    async fn get_gateway_block_info_by_number(&self, block_number: u64, block_hash: H256) -> Result<BlockInfo> {
        let bytes = self
            .get_gateway_object("block", block_number, &format!("{:?}", block_hash))
            .await?;
        let mut gz = flate2::read::GzDecoder::new(&bytes[..]);
        let mut decoded = Vec::new();
        use std::io::Read;
        gz.read_to_end(&mut decoded)?;
        Ok(serde_json::from_slice(&decoded)?)
    }

    async fn get_gateway_block_diff_by_number(&self, block_number: u64, state_root: H256) -> Result<BlockStorageDiff> {
        let bytes = self
            .get_gateway_object("stateDiff", block_number, &format!("{:?}", state_root))
            .await?;
        Ok(BlockStorageDiff::decode(&mut bytes.as_ref())?)
    }

    #[inline]
    async fn get_block_info(&self, block_hash: H256) -> Result<BlockInfo> {
        if let Some(block_ctx) = self.hash_to_blockctx.lock().unwrap().get(&block_hash) {
            return Ok(block_ctx.block_info.clone());
        }
        let cfg = self
            .kafka_s3_cfg
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("s3 object mode disabled"))?;
        s3_get_block_info(
            &self.s3_client,
            &cfg.bucket_name,
            &cfg.s3_chain_id,
            &cfg.version,
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
        let mut blockhash_to_block_info: HashMap<H256, BlockInfo> = HashMap::new();
        let mut roothash_to_block_info: HashMap<H256, BlockStorageDiff> = HashMap::new();

        for notif in &notifications {
            for new_block in &notif.new_blocks {
                let block_info = if self.gateway_object_cfg.is_some() {
                    self.get_gateway_block_info_by_number(new_block.block_number, new_block.hash)
                        .await?
                } else {
                    let cfg = self
                        .kafka_s3_cfg
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("s3 object mode disabled"))?;
                    s3_get_block_info(
                        &self.s3_client,
                        &cfg.bucket_name,
                        &cfg.s3_chain_id,
                        &cfg.version,
                        new_block.hash,
                    )
                    .await?
                };
                let parent_block_info = if let Some(parent) = blockhash_to_block_info.get(&new_block.parent_hash) {
                    parent.clone()
                } else if self.gateway_object_cfg.is_some() {
                    if new_block.block_number == 0 {
                        block_info.clone()
                    } else {
                        self.get_gateway_block_info_by_number(new_block.block_number - 1, new_block.parent_hash).await?
                    }
                } else {
                    self.get_block_info(new_block.parent_hash).await?
                };
                blockhash_to_block_info.insert(new_block.hash, block_info.clone());
                blockhash_to_block_info.entry(new_block.parent_hash).or_insert(parent_block_info.clone());
                if parent_block_info.header.state_root != block_info.header.state_root {
                    let block_diff = if self.gateway_object_cfg.is_some() {
                        self.get_gateway_block_diff_by_number(block_info.header.number, block_info.header.state_root).await?
                    } else {
                        let cfg = self
                            .kafka_s3_cfg
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("s3 object mode disabled"))?;
                        s3_get_block_diff(
                            &self.s3_client,
                            &cfg.bucket_name,
                            &cfg.s3_chain_id,
                            &cfg.version,
                            block_info.header.state_root,
                        )
                        .await?
                    };
                    roothash_to_block_info.insert(block_diff.hash, block_diff);
                }
            }
        }

        for mut notif in notifications {
            debug!(target:"updater", "get block_change_notification {:?}", notif);
            for new_block in notif.new_blocks.drain(..) {
                let parent_block_info = &blockhash_to_block_info[&new_block.parent_hash];
                let block_info = blockhash_to_block_info[&new_block.hash].clone();
                let block_diff = if parent_block_info.header.state_root == block_info.header.state_root {
                    let mut diff = BlockStorageDiff::default();
                    diff.hash = block_info.header.state_root;
                    diff.parent_hash = parent_block_info.header.state_root;
                    diff
                } else {
                    roothash_to_block_info[&block_info.header.state_root].clone()
                };
                self.hash_to_blockctx.lock().unwrap().insert(
                    new_block.hash,
                    BlockContextCache { block_diff, block_info },
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
        if self.gateway_object_cfg.is_some() {
            for block_number in start_block_number..=end_block_number {
                let block_info = self
                    .get_gateway_block_info_by_number(block_number, H256::ZERO)
                    .await?;
                let block_diff = self
                    .get_gateway_block_diff_by_number(block_number, block_info.header.state_root)
                    .await?;
                info!(target:"updater", "update block number {}, hash {}, parent hash {}", block_info.header.number, block_info.header.hash, block_info.header.parent_hash);
                self.tree.update_block(block_info, block_diff)?;
            }
            info!(target:"updater", "update from gateway, start block number {}, end block number {}", start_block_number, end_block_number);
            return Ok(());
        }

        let cfg = self
            .kafka_s3_cfg
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("s3 object mode disabled"))?;
        let mut get_block_info_diff_join_set = JoinSet::new();
        for block_number in start_block_number..=end_block_number {
            let rpc_client = self.rpc_client.clone();
            let client = self.s3_client.clone();
            let bucket_name = cfg.bucket_name.clone();
            let outer_bucket_name = cfg.outer_bucket_name.clone();
            let s3_chain_id = cfg.s3_chain_id.clone();
            let version = cfg.version.clone();
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
        let target_block_number = target_block.block_number.saturating_sub(1);
        let mut start_block_number = self.tree.last_committed_block()?.unwrap().header.number + 1;
        let batch_size = self.init_task_queue_size as u64;
        info!(target:"updater", "update from source, start block number {}, target block number {}", start_block_number, target_block_number);
        while start_block_number <= target_block_number {
            let end_block_number = std::cmp::min(start_block_number + batch_size - 1, target_block_number);
            self.update_range_from_s3(start_block_number, end_block_number).await?;
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
        use futures::{SinkExt, StreamExt};
        info!(target:"updater", "connecting to ws endpoint {}", self.ws_url);
        let (ws_stream, _resp) = connect_async(&self.ws_url).await?;
        let (mut sink, read) = ws_stream.split();
        let hello = self.build_hello_request()?;
        info!(target:"updater", "sending leafage hello chain={} last_block={} data_version={}", hello.chain_id, hello.resume_cursor.last_block_number, hello.resume_cursor.data_version);
        sink.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
        let mut chunks = read.ready_chunks(std::cmp::max(1, self.max_diff_depth));

        self.read_from_ws = false;

        while let Some(messages) = chunks.next().await {
            let mut notifications = Vec::with_capacity(messages.len());
            for message in messages {
                let message = message?;
                match self.handle_control_message(&message).await? {
                    ControlAction::Ignore => {}
                    ControlAction::Continue => continue,
                    ControlAction::Catchup { from_block, to_block } => {
                        if from_block <= to_block {
                            self.update_range_from_s3(from_block, to_block).await?;
                            let _ = self.prune_cache();
                        }
                        continue;
                    }
                    ControlAction::SnapshotRequired { manifest_url, reason, resume_from } => {
                        return Err(anyhow::anyhow!(
                            "snapshot restore not implemented yet: reason={}, manifest_url={}, resume_from={}",
                            reason,
                            manifest_url,
                            resume_from
                        ));
                    }
                }
                match message {
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
