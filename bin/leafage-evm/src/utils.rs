use alloy_rlp::Decodable;
use anyhow::Result;
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
    pub bucket_name: String,
    pub offset_dir: String,
}

pub async fn s3_get_block_diff(
    s3_client: &Client,
    bucket_name: &str,
    block_root: H256,
) -> Result<BlockStorageDiff> {
    let s3_key = format!("{}/stateDiff", block_root);
    let s3_obj = s3_client
        .get_object()
        .bucket(bucket_name)
        .key(&s3_key)
        .send()
        .await?;
    let bytes = s3_obj
        .body
        .bytes()
        .expect(&format!("Failed to get object {}", s3_key));
    let mut gz = read::GzDecoder::new(&bytes[..]);
    let mut bytes = Vec::new();
    gz.read_to_end(&mut bytes)?;
    let block_storage_diff = BlockStorageDiff::decode(&mut bytes.as_ref())?;
    Ok(block_storage_diff)
}

pub async fn s3_get_block_info(
    s3_client: &Client,
    bucket_name: &str,
    block_hash: H256,
) -> Result<Block<Transaction>> {
    let s3_key = format!("{}/block", block_hash);
    let s3_obj = s3_client
        .get_object()
        .bucket(bucket_name)
        .key(&s3_key)
        .send()
        .await?;
    let bytes = s3_obj
        .body
        .bytes()
        .expect(&format!("Failed to get object {}", s3_key));
    let mut gz = read::GzDecoder::new(&bytes[..]);
    let mut bytes = Vec::new();
    gz.read_to_end(&mut bytes)?;
    let block = serde_json::from_slice(&bytes)?;
    Ok(block)
}
