use alloy_rlp::Decodable;
use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use flate2::read;
use leafage_evm_types::{Block, BlockStorageDiff, Transaction, H256};
use serde::{Deserialize, Serialize};
use std::io::Read;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct KafkaS3Config {
    pub topic: String,
    pub brokers: String,
    pub partition: i32,
    pub start_offset: Option<i64>,
    pub bucket_name: String,
    pub offset_dir: String,
    pub s3_chain_id: String,
}

pub async fn s3_get_block_diff(
    s3_client: &Client,
    bucket_name: &str,
    s3_chain_id: &str,
    block_root: H256,
) -> Result<BlockStorageDiff> {
    let s3_key = format!("{}/{}/stateDiff", s3_chain_id, block_root);
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
    block_hash: H256,
) -> Result<Block<Transaction>> {
    let s3_key = format!("{}/{}/block", s3_chain_id, block_hash);
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
    let block = serde_json::from_slice(&bytes)?;
    Ok(block)
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
