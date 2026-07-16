use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use flate2::read;
use jsonrpsee::http_client::HttpClient;
use leafage_evm_rpc::EthApiClient;
use leafage_evm_storage::account_codec;
use leafage_evm_types::{decode_state_diff, BlockInfo, BlockStateUpdate, DebankTransaction, H256};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::num::NonZeroUsize;
use std::sync::LazyLock;
use std::sync::RwLock;
use std::{io::Read, str::FromStr};
use tracing::{debug, trace};

static S3_BLOCK_CACHE: LazyLock<RwLock<LruCache<H256, BlockInfo>>> =
    LazyLock::new(|| RwLock::new(LruCache::new(NonZeroUsize::new(1024).unwrap())));

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct KafkaS3Config {
    pub topic: String,
    pub brokers: String,
    pub partition: i32,
    pub bucket_name: String,
    pub outer_bucket_name: String,
    #[serde(default)]
    pub offset_dir: String,
    pub s3_chain_id: String,
    #[serde(default)]
    pub version: String,
}

/// Parse a `--state-diff-codec` CLI argument.
pub fn parse_state_diff_codec(arg: &str) -> Result<leafage_evm_types::StateDiffCodec> {
    match arg {
        "standard" => Ok(leafage_evm_types::StateDiffCodec::Standard),
        "blast-v1" => Ok(leafage_evm_types::StateDiffCodec::BlastV1),
        _ => Err(anyhow::anyhow!("unsupported state-diff codec: {arg}")),
    }
}

/// Parse a [`KafkaS3Config`] CLI argument: an absolute file path or inline JSON.
pub fn parse_kafka_s3_config(arg: &str) -> Result<KafkaS3Config> {
    let kafka_s3_config: KafkaS3Config;
    if arg.starts_with("/") {
        let file = std::fs::File::open(arg)?;
        kafka_s3_config = serde_json::from_reader(file)?;
    } else {
        kafka_s3_config = serde_json::from_str(arg)?;
    }
    Ok(kafka_s3_config)
}

/// Parse an `eth_getBlockBy*` JSON response into [`BlockInfo`].
///
/// alloy 的 `Block` 把 header 字段 `#[serde(flatten)]` 到顶层，而
/// `alloy_consensus::Header::mix_hash`（即 `mixHash`）是必填字段（没有
/// `#[serde(default)]`，也没有 `prevRandao` 别名）。部分链的 RPC 不返回
/// `mixHash`，会导致 "missing field `mixHash`" 的反序列化错误。
///
/// 合并后 `mixHash` 实际承载的是 prevRandao：`block_env_from_block` 在
/// difficulty 为 0 时会读取 `header.mix_hash` 作为 prevrandao。因此当
/// `mixHash` 缺失时，优先复用 RPC 返回的 `prevRandao`（避免丢失真实随机值），
/// 否则才退回零哈希（与 alloy `Header::default()` 一致，对 difficulty 非 0 的
/// 链无影响）。
fn parse_block_info(mut block: Value) -> Result<BlockInfo> {
    if let Some(obj) = block.as_object_mut() {
        if !obj.contains_key("mixHash") {
            let mix_hash = obj
                .get("prevRandao")
                .cloned()
                .unwrap_or_else(|| serde_json::to_value(H256::ZERO).expect("zero hash serializes"));
            obj.insert("mixHash".to_string(), mix_hash);
        }
    }
    serde_json::from_value(block).context("rpc get block by hash parse failed")
}

pub async fn s3_get_block_diff(
    s3_client: &Client,
    bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    block_root: H256,
) -> Result<BlockStateUpdate> {
    let s3_key = if version.is_empty() {
        format!("{}/{}/stateDiff", s3_chain_id, block_root)
    } else {
        format!("{}/{}/{}/stateDiff", s3_chain_id, version, block_root)
    };
    let s3_obj = s3_client
        .get_object()
        .bucket(bucket_name)
        .key(&s3_key)
        .send()
        .await?;
    let bytes = s3_obj.body.collect().await?.into_bytes();
    let block_storage_diff = decode_state_diff(account_codec(), bytes.as_ref())?;
    // Correlate with the commit-side logs in StateDBWrapper::update_block via
    // the state root. Enable with RUST_LOG=state_diff=debug (or =trace for
    // per-account / per-slot detail).
    debug!(target: "state_diff",
        "fetched stateDiff: root {}, parent_root {}, new_accounts {}, deleted_accounts {}, storage_accounts {}, storage_slots {}, new_codes {}",
        block_storage_diff.hash,
        block_storage_diff.parent_hash,
        block_storage_diff.new_accounts.len(),
        block_storage_diff.deleted_accounts.len(),
        block_storage_diff.storage_diffs.len(),
        block_storage_diff.storage_diffs.iter().map(|d| d.diffs.len()).sum::<usize>(),
        block_storage_diff.new_codes.len(),
    );
    for account in &block_storage_diff.new_accounts {
        trace!(target: "state_diff",
            "fetched account: root {}, address {}, nonce {}, code_hash {}",
            block_storage_diff.hash, account.address, account.account.nonce, account.account.code_hash);
    }
    for address in &block_storage_diff.deleted_accounts {
        trace!(target: "state_diff",
            "fetched deleted account: root {}, address {}", block_storage_diff.hash, address);
    }
    for account_diff in &block_storage_diff.storage_diffs {
        for pair in &account_diff.diffs {
            trace!(target: "state_diff",
                "fetched storage: root {}, address {}, index {}, value {}",
                block_storage_diff.hash, account_diff.address, pair.index, pair.value);
        }
    }
    Ok(block_storage_diff)
}

pub async fn s3_get_block_info(
    s3_client: &Client,
    bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    block_hash: H256,
) -> Result<BlockInfo> {
    if let Some(block) = S3_BLOCK_CACHE.read().unwrap().peek(&block_hash) {
        return Ok(block.clone());
    }
    let s3_key = if version.is_empty() {
        format!("{}/{}/block", s3_chain_id, block_hash)
    } else {
        format!("{}/{}/{}/block", s3_chain_id, version, block_hash)
    };
    let s3_obj = s3_client
        .get_object()
        .bucket(bucket_name)
        .key(&s3_key)
        .send()
        .await
        .context(format!("{bucket_name}: {s3_key}"))?;
    let bytes = s3_obj.body.collect().await?.into_bytes();
    let mut gz = read::GzDecoder::new(&bytes[..]);
    let mut bytes = Vec::new();
    gz.read_to_end(&mut bytes)?;
    let block: BlockInfo = serde_json::from_slice(&bytes)?;
    S3_BLOCK_CACHE
        .write()
        .unwrap()
        .put(block_hash, block.clone());
    Ok(block)
}

pub async fn s3_get_block_transactions(
    s3_client: &Client,
    bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    block_hash: H256,
) -> Result<Vec<DebankTransaction>> {
    let s3_key = if version.is_empty() {
        format!("{}/{}", s3_chain_id, block_hash)
    } else {
        format!("{}/{}/{}", s3_chain_id, version, block_hash)
    };
    let s3_obj = s3_client
        .get_object()
        .bucket(bucket_name)
        .key(&s3_key)
        .send()
        .await
        .context(format!("{bucket_name}: {s3_key}"))?;
    let bytes = s3_obj.body.collect().await?.into_bytes();
    let mut gz = read::GzDecoder::new(&bytes[..]);
    let mut bytes = Vec::new();
    gz.read_to_end(&mut bytes)?;
    let block_file: Value = serde_json::from_slice(&bytes)?;
    Ok(match block_file.get("txs").cloned() {
        None => Vec::new(),
        Some(txs) => serde_json::from_value(txs)?,
    })
}

pub async fn s3_get_block_transactions_by_number(
    rpc_client: &Option<HttpClient>,
    s3_client: &Client,
    outer_bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    number: u64,
) -> Result<Vec<DebankTransaction>> {
    let transactions = match rpc_client {
        Some(rpc) => {
            let block = rpc
                .get_block_by_number(number.into(), false)
                .await
                .context(format!("rpc get block by hash failed, {number}"))?;
            if block.is_none() {
                return Err(anyhow::anyhow!(
                    "rpc get block by hash returned none, {number}"
                ));
            }
            let block: BlockInfo = parse_block_info(block.unwrap())?;
            s3_get_block_transactions(
                s3_client,
                outer_bucket_name,
                s3_chain_id,
                version,
                block.header.hash,
            )
            .await
            .context(format!("s3 get transactions failed, {}", block.header.hash))?
        }
        None => {
            let block_hash = s3_get_block_hash_by_number(
                s3_client,
                outer_bucket_name,
                s3_chain_id,
                version,
                number,
            )
            .await?;
            s3_get_block_transactions(
                s3_client,
                outer_bucket_name,
                s3_chain_id,
                version,
                block_hash,
            )
            .await
            .context(format!("s3 get transactions failed, {block_hash}"))?
        }
    };
    Ok(transactions)
}

pub async fn s3_get_block_hash_by_number(
    s3_client: &Client,
    bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    number: u64,
) -> Result<H256> {
    #[derive(Clone, Debug, Default, Deserialize, Serialize)]
    #[serde(rename_all = "snake_case")]
    struct BlockValidation {
        pub validation_hash: i64,
        pub is_fork: bool,
    }
    let prefix = if version.is_empty() {
        format!("{}/{}/", s3_chain_id, number)
    } else {
        format!("{}/{}/{}/", s3_chain_id, version, number)
    };
    let list_output = s3_client
        .list_objects_v2()
        .bucket(bucket_name)
        .prefix(&prefix)
        .send()
        .await
        .context(format!(
            "Failed to list objects in bucket {bucket_name} with prefix {prefix}"
        ))?;
    // 只有一个对象，肯定没有fork，直接返回
    if list_output.contents().len() == 1 {
        let hash_str = list_output.contents()[0]
            .key()
            .unwrap()
            .strip_prefix(&prefix)
            .ok_or_else(|| anyhow::anyhow!("Failed to strip prefix {prefix} from key"))?;
        return H256::from_str(hash_str)
            .context(format!("Failed to parse block hash from key {hash_str}"));
    }
    for object in list_output.contents() {
        if let Some(key) = object.key() {
            let s3_obj = s3_client
                .get_object()
                .bucket(bucket_name)
                .key(key)
                .send()
                .await
                .context(format!("{bucket_name}: {key}"))?;
            let bytes = s3_obj.body.collect().await?.into_bytes();
            let mut gz = read::GzDecoder::new(&bytes[..]);
            let mut bytes = Vec::new();
            gz.read_to_end(&mut bytes)?;
            let block_validation: BlockValidation = serde_json::from_slice(&bytes)
                .context(format!("Failed to parse block validation"))?;
            if !block_validation.is_fork {
                let hash_str = key.strip_prefix(&prefix).ok_or_else(|| {
                    anyhow::anyhow!("Failed to strip prefix {prefix} from key {key}")
                })?;
                return H256::from_str(hash_str)
                    .context(format!("Failed to parse block hash from key {hash_str}"));
            }
        }
    }
    Err(anyhow::anyhow!(
        "No valid block hash found for number {number} in chain {s3_chain_id}"
    ))
}

/// Resolve a block number to its [`BlockInfo`], preferring the RPC client
/// when available and falling back to the S3 outer-bucket number index.
pub async fn s3_get_block_info_by_number(
    rpc_client: &Option<HttpClient>,
    s3_client: &Client,
    bucket_name: &str,
    outer_bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    number: u64,
) -> Result<BlockInfo> {
    match rpc_client {
        Some(rpc) => {
            let block = rpc
                .get_block_by_number(number.into(), false)
                .await
                .context(format!("rpc get block by hash failed, {number}"))?;
            if block.is_none() {
                return Err(anyhow::anyhow!(
                    "rpc get block by hash returned none, {number}"
                ));
            }
            parse_block_info(block.unwrap())
        }
        None => {
            let block_hash = s3_get_block_hash_by_number(
                s3_client,
                outer_bucket_name,
                s3_chain_id,
                version,
                number,
            )
            .await?;
            s3_get_block_info(s3_client, bucket_name, s3_chain_id, version, block_hash)
                .await
                .context(format!("s3 get block info failed, {block_hash}"))
        }
    }
}

/// Compute the [`BlockStateUpdate`] for an already-resolved [`BlockInfo`] by
/// fetching its parent (by hash) and comparing state roots: an unchanged root
/// yields an empty diff, otherwise the diff is read from S3.
async fn s3_resolve_block_diff(
    s3_client: &Client,
    bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    block_info: &BlockInfo,
) -> Result<BlockStateUpdate> {
    let parent_block_info = s3_get_block_info(
        s3_client,
        bucket_name,
        s3_chain_id,
        version,
        block_info.header.parent_hash,
    )
    .await
    .context(format!(
        "s3 get parent block info failed, {}",
        block_info.header.parent_hash
    ))?;
    if parent_block_info.header.state_root != block_info.header.state_root {
        s3_get_block_diff(
            s3_client,
            bucket_name,
            s3_chain_id,
            version,
            block_info.header.state_root,
        )
        .await
        .context(format!(
            "s3 get block diff failed, root: {}, number: {}",
            block_info.header.state_root, block_info.header.number
        ))
    } else {
        let mut diff = BlockStateUpdate::default();
        diff.hash = block_info.header.state_root;
        diff.parent_hash = parent_block_info.header.state_root;
        Ok(diff)
    }
}

pub async fn s3_get_block_info_and_diff_by_number(
    rpc_client: &Option<HttpClient>,
    s3_client: &Client,
    bucket_name: &str,
    outer_bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    number: u64,
) -> Result<(BlockInfo, BlockStateUpdate)> {
    let block_info = s3_get_block_info_by_number(
        rpc_client,
        s3_client,
        bucket_name,
        outer_bucket_name,
        s3_chain_id,
        version,
        number,
    )
    .await?;

    let block_diff =
        s3_resolve_block_diff(s3_client, bucket_name, s3_chain_id, version, &block_info).await?;
    Ok((block_info, block_diff))
}

/// Resolve a block to its [`BlockInfo`] and [`BlockStateUpdate`] strictly by
/// hash, following the by-hash S3 layout instead of the by-number index. Used
/// to backfill the chain tip along the exact parent-hash links carried by
/// Kafka, so a reorg near the tip cannot make the by-number index resolve a
/// sibling on the wrong branch.
pub async fn s3_get_block_info_and_diff_by_hash(
    s3_client: &Client,
    bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    hash: H256,
) -> Result<(BlockInfo, BlockStateUpdate)> {
    let block_info = s3_get_block_info(s3_client, bucket_name, s3_chain_id, version, hash).await?;
    let block_diff =
        s3_resolve_block_diff(s3_client, bucket_name, s3_chain_id, version, &block_info).await?;
    Ok((block_info, block_diff))
}

pub async fn s3_get_block_info_and_diff_by_number_for_genesis(
    rpc_client: &Option<HttpClient>,
    s3_client: &Client,
    bucket_name: &str,
    outer_bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    number: u64,
) -> Result<(BlockInfo, BlockStateUpdate)> {
    let block_info = s3_get_block_info_by_number(
        rpc_client,
        s3_client,
        bucket_name,
        outer_bucket_name,
        s3_chain_id,
        version,
        number,
    )
    .await?;
    let block_diff = s3_get_block_diff(
        s3_client,
        bucket_name,
        s3_chain_id,
        version,
        block_info.header.state_root,
    )
    .await
    .context(format!(
        "s3 get block diff failed, root: {}, number: {}",
        block_info.header.state_root, number
    ))?;
    Ok((block_info, block_diff))
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EtcdRegisterConfig {
    pub endpoints: Vec<String>,
    pub keep_alive_interval_ms: u64,
    pub lease_ttl_s: i64,
    #[serde(default)]
    pub meta: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub state_type: u64,
    pub address: String,
    pub port: u64,
    pub node_type: u64,
}

#[derive(Debug, Clone)]
#[repr(u64)]
#[allow(dead_code)]
pub enum StateType {
    Latest = 1,
    Delay = 2,
    Offline = 3,
}

#[derive(Debug, Clone, Copy)]
#[repr(u64)]
pub enum NodeType {
    State = 1,
    Archive = 2,
}

/// CLI selector for the node type registered to etcd. `Auto` (the default)
/// preserves the historical behavior of deriving the type from `--archive`;
/// `State`/`Archive` override it explicitly.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum NodeTypeArg {
    /// Derive from `--archive`: archive nodes register as archive, all
    /// others as state.
    #[default]
    Auto,
    /// Register as a state node (node_type = 1) regardless of `--archive`.
    State,
    /// Register as an archive node (node_type = 2) regardless of `--archive`.
    Archive,
}

impl NodeTypeArg {
    /// Resolve to the concrete [`NodeType`] written to etcd, falling back to
    /// `is_archive` when `Auto`.
    pub fn resolve(self, is_archive: bool) -> NodeType {
        match self {
            NodeTypeArg::Auto if is_archive => NodeType::Archive,
            NodeTypeArg::Auto => NodeType::State,
            NodeTypeArg::State => NodeType::State,
            NodeTypeArg::Archive => NodeType::Archive,
        }
    }
}
