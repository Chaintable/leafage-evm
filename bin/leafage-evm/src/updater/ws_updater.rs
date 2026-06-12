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
use metrics::{counter, gauge, histogram};
use std::str::FromStr;

#[derive(Debug, Clone)]
struct BlockContextCache {
    block_diff: BlockStorageDiff,
    block_info: BlockInfo,
}


enum ControlAction {
    Ignore,
    Continue,
    Catchup { from_block: u64, to_block: u64, manifest: Vec<LeafageManifestEntry> },
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

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct OfficialHelloRequest {
    r#type: String,
    protocol_version: String,
    chain_id: String,
    client_type: String,
    gateway_id: String,
    api_key: String,
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
    #[serde(default)]
    protocol_version: String,
    #[serde(default)]
    accepted_protocol_version: String,
    chain_id: String,
    current_data_version: String,
    latest_height: u64,
    #[serde(default)]
    next_action: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct LeafageManifestEntry {
    block_number: u64,
    block_hash: String,
    state_root: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct CanonicalIndexEntry {
    block_hash: String,
    state_root: String,
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
    #[serde(default)]
    manifest: Vec<LeafageManifestEntry>,
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

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct LeafageCatchupComplete {
    r#type: String,
    chain_id: String,
    current_data_version: String,
    last_block_number: u64,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct PassthroughFrameMeta {
    r#type: String,
    seq: u64,
    chain_id: String,
    data_version: String,
    min_block: u64,
    max_block: u64,
    ts: i64,
}

fn decode_passthrough_notification(frame: &[u8]) -> Result<(PassthroughFrameMeta, KafkaBlockChangeNotification)> {
    if frame.len() < 4 {
        return Err(anyhow::anyhow!("passthrough frame too short"));
    }
    let meta_len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;
    if frame.len() < 4 + meta_len {
        return Err(anyhow::anyhow!("passthrough frame truncated"));
    }
    let meta: PassthroughFrameMeta = serde_json::from_slice(&frame[4..4 + meta_len])?;
    if meta.r#type != "kafka_passthrough" {
        return Err(anyhow::anyhow!("unsupported passthrough frame type: {}", meta.r#type));
    }
    let raw_payload = &frame[4 + meta_len..];
    let mut gz = flate2::read::GzDecoder::new(raw_payload);
    let mut decoded = Vec::new();
    use std::io::Read;
    gz.read_to_end(&mut decoded)?;
    let notif: KafkaBlockChangeNotification = serde_json::from_slice(&decoded)?;
    Ok((meta, notif))
}


fn metric_reconnect_reason(err: &anyhow::Error) -> String {
    let msg = err.to_string().to_lowercase();
    if msg.contains("reset") {
        "connection_reset".to_string()
    } else if msg.contains("refused") {
        "connection_refused".to_string()
    } else if msg.contains("lookup") {
        "dns".to_string()
    } else if msg.contains("timeout") {
        "timeout".to_string()
    } else if msg.contains("snapshot restore not implemented") {
        "snapshot_required".to_string()
    } else {
        "other".to_string()
    }
}

fn record_ws_frame(direction: &str, frame_type: &str, size: usize) {
    counter!("pipeline_leafage_ws_frames_total", &[("direction", direction.to_string()), ("type", frame_type.to_string())]).increment(1);
    counter!("pipeline_leafage_ws_frame_bytes_total", &[("direction", direction.to_string()), ("type", frame_type.to_string())]).increment(size as u64);
}

fn record_next_action(action: &str, source: &str) {
    counter!("pipeline_leafage_next_action_total", &[("action", action.to_string()), ("source", source.to_string())]).increment(1);
}

fn set_progress(stage: &str) {
    gauge!("pipeline_leafage_last_progress_timestamp_seconds", &[("stage", stage.to_string())]).set(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64());
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
        let shared_config = aws_config::load_from_env().await;
        let s3_client = if let Some(cfg) = &gateway_object_cfg {
            if !cfg.r2_endpoint.is_empty() {
                let s3_conf = aws_sdk_s3::config::Builder::from(&shared_config)
                    .force_path_style(true)
                    .endpoint_url(cfg.r2_endpoint.clone())
                    .build();
                aws_sdk_s3::Client::from_conf(s3_conf)
            } else {
                aws_sdk_s3::Client::new(&shared_config)
            }
        } else {
            aws_sdk_s3::Client::new(&shared_config)
        };

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

    fn manifest_prefetch_window(&self) -> usize {
        self.gateway_object_cfg
            .as_ref()
            .map(|cfg| cfg.prefetch_window.max(1))
            .unwrap_or(8)
    }

    fn ws_protocol(&self) -> &str {
        self.gateway_object_cfg
            .as_ref()
            .map(|cfg| cfg.ws_protocol.as_str())
            .filter(|protocol| *protocol == "official")
            .unwrap_or("leafage")
    }

    fn is_official_ws(&self) -> bool {
        self.ws_protocol() == "official"
    }

    fn gateway_object_fetch_mode(cfg: &GatewayObjectConfig) -> &str {
        match cfg.object_fetch_mode.as_str() {
            "url" | "s3" | "auto" => cfg.object_fetch_mode.as_str(),
            _ => "auto",
        }
    }

    fn gateway_snapshot_url(&self) -> Option<String> {
        let cfg = self.gateway_object_cfg.as_ref()?;
        let base = cfg.base_url.trim_end_matches('/');
        Some(format!(
            "{}/v1/snapshot?chain_id={}&version={}&is_archive={}",
            base, cfg.chain_id, cfg.version, cfg.snapshot_is_archive
        ))
    }

    fn r2_object_url(cfg: &GatewayObjectConfig, bucket: &str, key: &str) -> Result<String> {
        let base = cfg.r2_public_base_url.trim_end_matches('/');
        if base.is_empty() {
            return Err(anyhow::anyhow!("r2_public_base_url is not configured"));
        }
        let escaped_key = key
            .split('/')
            .map(|part| part.replace('%', "%25").replace(' ', "%20").replace('#', "%23").replace('?', "%3F"))
            .collect::<Vec<_>>()
            .join("/");
        Ok(format!("{}/{}/{}", base, bucket, escaped_key))
    }

    async fn r2_get_object_by_url(cfg: &GatewayObjectConfig, bucket: &str, key: &str) -> Result<Vec<u8>> {
        let url = Self::r2_object_url(cfg, bucket, key)?;
        let resp = reqwest::get(&url)
            .await
            .context(format!("r2 url fetch failed: {}", url))?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("r2 url fetch failed status {} for {}", resp.status(), url));
        }
        Ok(resp.bytes().await?.to_vec())
    }

    async fn r2_get_object_by_s3(s3_client: &Client, bucket: &str, key: &str) -> Result<Vec<u8>> {
        let s3_obj = s3_client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .context(format!("{bucket}: {key}"))?;
        Ok(s3_obj.body.collect().await?.into_bytes().to_vec())
    }

    async fn r2_get_object(cfg: &GatewayObjectConfig, s3_client: &Client, bucket: &str, key: &str) -> Result<Vec<u8>> {
        match Self::gateway_object_fetch_mode(cfg) {
            "url" => Self::r2_get_object_by_url(cfg, bucket, key).await,
            "s3" => Self::r2_get_object_by_s3(s3_client, bucket, key).await,
            _ => {
                if !cfg.r2_public_base_url.is_empty() {
                    if let Ok(bytes) = Self::r2_get_object_by_url(cfg, bucket, key).await {
                        return Ok(bytes);
                    }
                }
                Self::r2_get_object_by_s3(s3_client, bucket, key).await
            }
        }
    }

    async fn r2_get_block_info_direct(cfg: &GatewayObjectConfig, s3_client: &Client, block_hash: H256) -> Result<BlockInfo> {
        let key = if cfg.version.is_empty() {
            format!("{}/{}/block", cfg.chain_id, block_hash)
        } else {
            format!("{}/{}/{}/block", cfg.chain_id, cfg.version, block_hash)
        };
        let bytes = Self::r2_get_object(cfg, s3_client, &cfg.r2_inner_bucket, &key).await?;
        let mut gz = flate2::read::GzDecoder::new(&bytes[..]);
        let mut decoded = Vec::new();
        use std::io::Read;
        gz.read_to_end(&mut decoded)?;
        Ok(serde_json::from_slice(&decoded)?)
    }

    async fn r2_get_block_diff_direct(cfg: &GatewayObjectConfig, s3_client: &Client, state_root: H256) -> Result<BlockStorageDiff> {
        let key = if cfg.version.is_empty() {
            format!("{}/{}/stateDiff", cfg.chain_id, state_root)
        } else {
            format!("{}/{}/{}/stateDiff", cfg.chain_id, cfg.version, state_root)
        };
        let bytes = Self::r2_get_object(cfg, s3_client, &cfg.r2_inner_bucket, &key).await?;
        Ok(BlockStorageDiff::decode(&mut bytes.as_ref())?)
    }

    async fn r2_get_canonical_index_direct(
        cfg: &GatewayObjectConfig,
        s3_client: &Client,
        block_number: u64,
    ) -> Result<CanonicalIndexEntry> {
        let key = if cfg.version.is_empty() {
            format!("{}/index/block/{}.json", cfg.chain_id, block_number)
        } else {
            format!("{}/{}/index/block/{}.json", cfg.chain_id, cfg.version, block_number)
        };
        let bytes = Self::r2_get_object(cfg, s3_client, &cfg.r2_outer_bucket, &key).await?;
        let mut gz = flate2::read::GzDecoder::new(&bytes[..]);
        let mut decoded = Vec::new();
        use std::io::Read;
        gz.read_to_end(&mut decoded)?;
        Ok(serde_json::from_slice(&decoded)?)
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

    fn build_official_hello_request(&self) -> Result<OfficialHelloRequest> {
        let leafage_hello = self.build_hello_request()?;
        let cfg = self
            .gateway_object_cfg
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("official ws requires gateway object config"))?;
        if cfg.api_key.trim().is_empty() {
            return Err(anyhow::anyhow!("official ws requires gateway object api_key"));
        }
        Ok(OfficialHelloRequest {
            r#type: "hello".to_string(),
            protocol_version: "v1".to_string(),
            chain_id: leafage_hello.chain_id,
            client_type: "leafage-evm".to_string(),
            gateway_id: leafage_hello.node_id,
            api_key: cfg.api_key.clone(),
            resume_cursor: leafage_hello.resume_cursor,
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
                        gauge!("pipeline_leafage_gateway_latest_height").set(ack.latest_height as f64);
                        record_next_action(&ack.next_action, "hello_ack");
                        info!(target:"updater", "leafage ws hello_ack chain={} version={} latest_height={} next_action={}", ack.chain_id, ack.current_data_version, ack.latest_height, ack.next_action);
                        if self.is_official_ws() {
                            let last_block_number = self.tree.last_committed_block()?
                                .map(|block| block.header.number)
                                .unwrap_or(0);
                            if ack.latest_height > last_block_number {
                                return Ok(ControlAction::Catchup {
                                    from_block: last_block_number.saturating_add(1),
                                    to_block: ack.latest_height,
                                    manifest: Vec::new(),
                                });
                            }
                            return Ok(ControlAction::Continue);
                        }
                        match ack.next_action.as_str() {
                            "replay" => Ok(ControlAction::Continue),
                            "catchup" => Ok(ControlAction::Continue),
                            "wait" => Ok(ControlAction::Continue),
                            "snapshot" => Ok(ControlAction::SnapshotRequired {
                                manifest_url: self.gateway_snapshot_url().unwrap_or_default(),
                                reason: "snapshot_required_from_hello_ack".to_string(),
                                resume_from: ack.latest_height,
                            }),
                            _ => Ok(ControlAction::Continue),
                        }
                    }
                    "catchup_required" => {
                        let control: LeafageCatchupRequired = serde_json::from_value(value)?;
                        info!(target:"updater", "leafage ws catchup_required chain={} version={} from_block={} to_block={} reason={}", control.chain_id, control.current_data_version, control.from_block, control.to_block, control.reason);
                        record_next_action("catchup", "control");
                        counter!("pipeline_leafage_catchup_blocks_total", &[("mode", if control.manifest.is_empty() { "s3".to_string() } else { "manifest".to_string() })]).increment((control.to_block.saturating_sub(control.from_block) + 1) as u64);
                        Ok(ControlAction::Catchup {
                            from_block: control.from_block,
                            to_block: control.to_block,
                            manifest: control.manifest,
                        })
                    }
                    "snapshot_required" => {
                        let control: LeafageSnapshotRequired = serde_json::from_value(value)?;
                        info!(target:"updater", "leafage ws snapshot_required chain={} version={} reason={} manifest={} resume_from={}", control.chain_id, control.current_data_version, control.reason, control.snapshot.manifest_url, control.snapshot.resume_from);
                        record_next_action("snapshot", "control");
                        Ok(ControlAction::SnapshotRequired {
                            manifest_url: if control.snapshot.manifest_url.is_empty() {
                                self.gateway_snapshot_url().unwrap_or_default()
                            } else {
                                control.snapshot.manifest_url
                            },
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

    async fn get_object_block_info_by_hash(&self, block_number: u64, block_hash: H256) -> Result<BlockInfo> {
        if self.is_official_ws() {
            let cfg = self
                .gateway_object_cfg
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("gateway object mode disabled"))?;
            Self::r2_get_block_info_direct(cfg, &self.s3_client, block_hash)
                .await
                .with_context(|| format!("r2 block info block {} hash {:?}", block_number, block_hash))
        } else {
            self.get_gateway_block_info_by_number(block_number, block_hash).await
        }
    }

    async fn get_object_block_diff_by_state_root(
        &self,
        block_number: u64,
        state_root: H256,
    ) -> Result<BlockStorageDiff> {
        if self.is_official_ws() {
            let cfg = self
                .gateway_object_cfg
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("gateway object mode disabled"))?;
            Self::r2_get_block_diff_direct(cfg, &self.s3_client, state_root)
                .await
                .with_context(|| format!("r2 block diff block {} state_root {:?}", block_number, state_root))
        } else {
            self.get_gateway_block_diff_by_number(block_number, state_root).await
        }
    }

    async fn update_range_from_manifest(&mut self, manifest: &[LeafageManifestEntry]) -> Result<()> {
        let batch_size = self.manifest_prefetch_window();
        let total = manifest.len();
        if total == 0 {
            return Ok(());
        }

        let cfg = self
            .gateway_object_cfg
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gateway object mode disabled"))?
            .clone();

        let mut start = 0usize;
        while start < total {
            let end = std::cmp::min(start + batch_size, total);
            let mut join_set = JoinSet::new();
            for idx in start..end {
                let entry = manifest[idx].clone();
                let s3_client = self.s3_client.clone();
                let cfg = cfg.clone();
                join_set.spawn(async move {
                    let block_hash = H256::from_str(&entry.block_hash)?;
                    let state_root = H256::from_str(&entry.state_root)?;
                    let block_info = Self::r2_get_block_info_direct(&cfg, &s3_client, block_hash).await?;
                    let block_diff = Self::r2_get_block_diff_direct(&cfg, &s3_client, state_root).await?;
                    Ok::<(usize, BlockInfo, BlockStorageDiff), anyhow::Error>((idx, block_info, block_diff))
                });
            }
            let mut batch_results = join_set.join_all().await;
            batch_results.sort_by_key(|res| match res {
                Ok((idx, _, _)) => *idx,
                Err(_) => usize::MAX,
            });
            for result in batch_results {
                let (_, block_info, block_diff) = result?;
                self.tree.update_block(block_info.clone(), block_diff)?;
                info!(target:"updater", "update block number {}, hash {:?}, parent hash {:?}", block_info.header.number, block_info.header.hash, block_info.header.parent_hash);
            }
            start = end;
        }
        Ok(())
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

    fn clear(&self, persist_block_num: u64, _persist_block_hash: H256) {
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
        let last_committed_block = self.tree.last_committed_block()?;
        if let Some(last_block) = &last_committed_block {
            blockhash_to_block_info.insert(last_block.header.hash, last_block.clone());
        }

        for notif in &notifications {
            for new_block in &notif.new_blocks {
                let block_info = if self.gateway_object_cfg.is_some() {
                    self.get_object_block_info_by_hash(new_block.block_number, new_block.hash).await?
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
                let cached_parent_ctx = {
                    self.hash_to_blockctx
                        .lock()
                        .unwrap()
                        .get(&new_block.parent_hash)
                        .cloned()
                };
                let parent_block_info = if let Some(parent) = blockhash_to_block_info.get(&new_block.parent_hash) {
                    parent.clone()
                } else if let Some(parent_ctx) = cached_parent_ctx {
                    parent_ctx.block_info
                } else if let Some(last_block) = &last_committed_block {
                    if last_block.header.hash == new_block.parent_hash {
                        last_block.clone()
                    } else if self.gateway_object_cfg.is_some() {
                        if new_block.block_number == 0 {
                            block_info.clone()
                        } else {
                            self.get_object_block_info_by_hash(new_block.block_number - 1, new_block.parent_hash).await?
                        }
                    } else {
                        self.get_block_info(new_block.parent_hash).await?
                    }
                } else if self.gateway_object_cfg.is_some() {
                    if new_block.block_number == 0 {
                        block_info.clone()
                    } else {
                        self.get_object_block_info_by_hash(new_block.block_number - 1, new_block.parent_hash).await?
                    }
                } else {
                    self.get_block_info(new_block.parent_hash).await?
                };
                blockhash_to_block_info.insert(new_block.hash, block_info.clone());
                blockhash_to_block_info.entry(new_block.parent_hash).or_insert(parent_block_info.clone());
                if parent_block_info.header.state_root != block_info.header.state_root {
                    let block_diff = if self.gateway_object_cfg.is_some() {
                        self.get_object_block_diff_by_state_root(block_info.header.number, block_info.header.state_root).await?
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
        if let Some(cfg) = &self.gateway_object_cfg {
            let cfg = cfg.clone();
            let batch_size = self.manifest_prefetch_window() as u64;
            let mut batch_start = start_block_number;
            while batch_start <= end_block_number {
                let batch_end = std::cmp::min(
                    batch_start.saturating_add(batch_size).saturating_sub(1),
                    end_block_number,
                );
                let mut join_set = JoinSet::new();
                for block_number in batch_start..=batch_end {
                    let cfg = cfg.clone();
                    let s3_client = self.s3_client.clone();
                    join_set.spawn(async move {
                        let index =
                            Self::r2_get_canonical_index_direct(&cfg, &s3_client, block_number)
                                .await
                                .with_context(|| format!("r2 canonical index block {}", block_number))?;
                        let block_hash = H256::from_str(&index.block_hash)
                            .with_context(|| format!("parse block hash for block {}", block_number))?;
                        let state_root = H256::from_str(&index.state_root)
                            .with_context(|| format!("parse state root for block {}", block_number))?;
                        let block_info = Self::r2_get_block_info_direct(&cfg, &s3_client, block_hash)
                            .await
                            .with_context(|| format!("r2 block info block {}", block_number))?;
                        let block_diff = Self::r2_get_block_diff_direct(&cfg, &s3_client, state_root)
                            .await
                            .with_context(|| format!("r2 block diff block {}", block_number))?;
                        Ok::<(u64, BlockInfo, BlockStorageDiff), anyhow::Error>((
                            block_number,
                            block_info,
                            block_diff,
                        ))
                    });
                }

                let mut batch_results = join_set.join_all().await;
                batch_results.sort_by_key(|res| match res {
                    Ok((block_number, _, _)) => *block_number,
                    Err(_) => u64::MAX,
                });
                for result in batch_results {
                    let (_, block_info, block_diff) = result?;
                    info!(target:"updater", "update block number {}, hash {}, parent hash {}", block_info.header.number, block_info.header.hash, block_info.header.parent_hash);
                    self.tree.update_block(block_info, block_diff)?;
                }
                batch_start = batch_end.saturating_add(1);
            }
            info!(target:"updater", "update from r2, start block number {}, end block number {}, prefetch window {}", start_block_number, end_block_number, batch_size);
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

    async fn catchup_gap_before_ws(&self, notifications: &[KafkaBlockChangeNotification]) -> Result<()> {
        let target_block = notifications
            .first()
            .and_then(|n| n.new_blocks.first())
            .ok_or_else(|| anyhow::anyhow!("No new blocks in the message"))?;
        if self
            .tree
            .state_at(leafage_evm_types::BlockId::Hash(target_block.parent_hash.into()))?
            .is_some()
        {
            return Ok(());
        }
        let last_committed = self.tree.last_committed_block()?.unwrap().header.number;
        info!(
            target:"updater",
            "ws parent gap detected, catchup before apply, last_committed={}, incoming_block={}, parent_hash={}",
            last_committed,
            target_block.block_number,
            target_block.parent_hash,
        );
        self.update_from_s3(notifications).await
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
            let apply_started = std::time::Instant::now();
            let block_storage_diff = block.block_diff;
            let block_info = block.block_info;
            let block_hash = block_info.header.hash;
            let block_num = block_info.header.number;
            let new_accounts_num = block_storage_diff.new_accounts.len();
            let deleted_accounts_num = block_storage_diff.deleted_accounts.len();
            let new_codes_num = block_storage_diff.new_codes.len();
            self.tree.update_block(block_info, block_storage_diff)?;
            histogram!("pipeline_leafage_apply_block_duration_seconds").record(apply_started.elapsed().as_secs_f64());
            gauge!("pipeline_leafage_applied_block").set(block_num as f64);
            set_progress("applied_block");
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

    /// Run one connection lifecycle. Returns when the peer closes or the
    /// stream errors. Every inbound WS batch verifies its parent is reachable;
    /// if not, the missing range is backfilled from R2 before applying live data.
    async fn serve(&mut self) -> Result<()> {
        use futures::{SinkExt, StreamExt};
        info!(target:"updater", "connecting to ws endpoint {}", self.ws_url);
        let (ws_stream, _resp) = connect_async(&self.ws_url).await?;
        gauge!("pipeline_leafage_ws_connection_up").set(1.0);
        set_progress("ws_connected");
        let (mut sink, read) = ws_stream.split();
        if self.is_official_ws() {
            let hello = self.build_official_hello_request()?;
            info!(target:"updater", "sending official hello chain={} last_block={} data_version={}", hello.chain_id, hello.resume_cursor.last_block_number, hello.resume_cursor.data_version);
            sink.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
        } else {
            let hello = self.build_hello_request()?;
            info!(target:"updater", "sending leafage hello chain={} last_block={} data_version={}", hello.chain_id, hello.resume_cursor.last_block_number, hello.resume_cursor.data_version);
            sink.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
        }
        let mut chunks = read.ready_chunks(std::cmp::max(1, self.max_diff_depth));

        self.read_from_ws = false;

        while let Some(messages) = chunks.next().await {
            let mut notifications = Vec::with_capacity(messages.len());
            for message in messages {
                let message = message?;
                match self.handle_control_message(&message).await? {
                    ControlAction::Ignore => {}
                    ControlAction::Continue => continue,
                    ControlAction::Catchup { from_block, to_block, manifest } => {
                        if from_block <= to_block {
                            if !manifest.is_empty() {
                                self.update_range_from_manifest(&manifest).await?;
                            } else {
                                self.update_range_from_s3(from_block, to_block).await?;
                            }
                            let _ = self.prune_cache();
                            if !self.is_official_ws() {
                                let last_block_number = self.tree.last_committed_block()?
                                    .map(|block| block.header.number)
                                    .unwrap_or(0);
                                let complete = LeafageCatchupComplete {
                                    r#type: "catchup_complete".to_string(),
                                    chain_id: self.chain_id().to_string(),
                                    current_data_version: self.data_version().to_string(),
                                    last_block_number,
                                };
                                sink.send(Message::Text(serde_json::to_string(&complete)?.into())).await?;
                            }
                            self.read_from_ws = true;
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
                        record_ws_frame("inbound", "control_json", text.len());
                        let notif: KafkaBlockChangeNotification = serde_json::from_str(&text)?;
                        notifications.push(notif);
                    }
                    Message::Binary(bytes) => {
                        record_ws_frame("inbound", "binary", bytes.len());
                        match decode_passthrough_notification(bytes.as_ref()) {
                            Ok((meta, notif)) => {
                                histogram!("pipeline_leafage_gateway_frame_latency_seconds").record((std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64() - meta.ts as f64).max(0.0));
                                notifications.push(notif)
                            },
                            Err(_) => {
                                counter!("pipeline_leafage_ws_decode_errors_total", &[("type", "passthrough_binary".to_string())]).increment(1);
                                let notif: KafkaBlockChangeNotification = bytes.as_ref().try_into()?;
                                notifications.push(notif);
                            }
                        }
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

            loop {
                if let Err(e) = self.catchup_gap_before_ws(&notifications).await {
                    error!(target:"updater", "Failed to catch up before ws apply: {:?}", e);
                    time::sleep(Duration::from_secs(1)).await;
                } else {
                    break;
                }
            }
            if !self.read_from_ws {
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
                        gauge!("pipeline_leafage_ws_connection_up").set(0.0);
                        match res {
                            Ok(()) => info!(target:"updater", "ws stream ended, reconnecting in 1s"),
                            Err(e) => {
                                counter!("pipeline_leafage_ws_reconnect_total", &[("reason", metric_reconnect_reason(&e))]).increment(1);
                                error!(target:"updater", "ws updater error: {:?}, reconnecting in 1s", e)
                            },
                        }
                        time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
        tx
    }
}
