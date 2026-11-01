//! Complete replay inputs from RPC. The analytical blockfile's reduced `txs`
//! cannot represent access lists, Celo fee currencies or consensus randomness.
use alloy::primitives::B256;
use anyhow::{Context as _, Result};
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

const MAX_INPUT: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct ReplayDataSource(HttpClient);

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ReplayData {
    pub version: u32,
    pub block: Value,
    pub receipts: Vec<Value>,
}

impl ReplayDataSource {
    pub fn rpc(url: &str) -> Result<Self> {
        Ok(Self(
            HttpClientBuilder::default()
                .request_timeout(Duration::from_secs(30))
                .max_response_size(MAX_INPUT as u32)
                .build(url)?,
        ))
    }

    pub(crate) async fn fetch(&self, hash: B256) -> Result<ReplayData> {
        let client = &self.0;
        let (block, receipts): (Option<Value>, Option<Vec<Value>>) = tokio::try_join!(
            client.request("eth_getBlockByHash", rpc_params![hash, true]),
            client.request("eth_getBlockReceipts", rpc_params![hash]),
        )?;
        let data = ReplayData {
            version: 1,
            block: block.context("upstream block is missing")?,
            receipts: receipts.context("upstream receipts are missing")?,
        };
        Ok(data)
    }
}
