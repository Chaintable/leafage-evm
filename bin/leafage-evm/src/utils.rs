use alloy_rlp::Decodable;
use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use flate2::read;
use jsonrpsee::http_client::HttpClient;
use leafage_evm_rpc::EthApiClient;
use leafage_evm_types::{Block, BlockStorageDiff, DebankTransaction, H256};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::num::NonZeroUsize;
use std::sync::LazyLock;
use std::sync::RwLock;
use std::{io::Read, str::FromStr};

static S3_BLOCK_CACHE: LazyLock<RwLock<LruCache<H256, Block<H256>>>> =
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

pub async fn s3_get_block_diff(
    s3_client: &Client,
    bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    block_root: H256,
) -> Result<BlockStorageDiff> {
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
    let block_storage_diff = BlockStorageDiff::decode(&mut bytes.as_ref())?;
    Ok(block_storage_diff)
}

pub async fn s3_get_block_info(
    s3_client: &Client,
    bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    block_hash: H256,
) -> Result<Block<H256>> {
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
    let block: Block<H256> = serde_json::from_slice(&bytes)?;
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
            let block: Block<H256> = serde_json::from_value(block.unwrap())
                .context("rpc get block by hash parse failed")?;
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

pub async fn s3_get_block_info_and_diff_by_number(
    rpc_client: &Option<HttpClient>,
    s3_client: &Client,
    bucket_name: &str,
    outer_bucket_name: &str,
    s3_chain_id: &str,
    version: &str,
    number: u64,
) -> Result<(Block<H256>, BlockStorageDiff)> {
    let block_info = match rpc_client {
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
            let block: Block<H256> = serde_json::from_value(block.unwrap())
                .context("rpc get block by hash parse failed")?;
            block
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
                .context(format!("s3 get block info failed, {block_hash}"))?
        }
    };

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
    let block_diff = if parent_block_info.header.state_root != block_info.header.state_root {
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
            block_info.header.state_root, number
        ))?
    } else {
        let mut diff = BlockStorageDiff::default();
        diff.hash = block_info.header.state_root;
        diff.parent_hash = parent_block_info.header.state_root;
        diff
    };
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
) -> Result<(Block<H256>, BlockStorageDiff)> {
    let block_info = match rpc_client {
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
            let block: Block<H256> = serde_json::from_value(block.unwrap())
                .context("rpc get block by hash parse failed")?;
            block
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
                .context(format!("s3 get block info failed, {block_hash}"))?
        }
    };
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

#[derive(Debug, Clone)]
#[repr(u64)]
pub enum NodeType {
    State = 1,
    Archive = 2,
}
