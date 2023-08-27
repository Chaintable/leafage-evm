use anyhow::Result;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::{EthApiClient, LeafAgeApiClient};
use leafage_evm_storage::{BlockContext, EvmStorageRead, EvmStorageWrite, SnapshotTree, StateDB};
use leafage_evm_types::{Block, BlockId, BlockNumber, BlockStorageDiff, Transaction};
use open_fastrlp::Decodable;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{error, info};

pub struct Updater<DB> {
    rpc_client: HttpClient,
    snaps: Arc<SnapshotTree<DB>>,
    block_queue: VecDeque<Block<Transaction>>,
}

impl<DB> Updater<DB>
where
    DB: StateDB
        + EvmStorageWrite<Error = <DB as StateDB>::Error>
        + BlockContext<Error = <DB as StateDB>::Error>
        + Send
        + Sync
        + 'static,
{
    pub fn new(snaps: Arc<SnapshotTree<DB>>, rpc_url: impl AsRef<str>) -> Result<Self> {
        let rpc_client = HttpClientBuilder::default().build(rpc_url)?;
        Ok(Self {
            rpc_client,
            snaps,
            block_queue: VecDeque::new(),
        })
    }

    async fn update(&mut self) -> Result<bool> {
        if self.block_queue.is_empty() {
            let current_block_info = self.snaps.block_info()?;
            let next_block_number =
                BlockNumber::Number((current_block_info.number.unwrap().as_u64() + 1).into());
            let next_block_info = self
                .rpc_client
                .get_block_by_number(next_block_number, true)
                .await?;
            if next_block_info.is_none() {
                info!(target:"updater", "no new block");
                return Ok(false);
            } else {
                let next_block_info: Block<Transaction> =
                    serde_json::from_value(next_block_info.unwrap())?;
                self.block_queue.push_back(next_block_info);
            }
        }
        loop {
            let first_block_info = self.block_queue.front().unwrap();
            if self
                .snaps
                .state_at(BlockId::Hash(first_block_info.parent_hash))?
                .is_some()
            {
                break;
            }
            let parent_block_info = self
                .rpc_client
                .get_block_by_hash(first_block_info.parent_hash, true)
                .await?;
            if parent_block_info.is_none() {
                info!(target:"updater", "can't not find block {}", first_block_info.parent_hash);
                return Ok(false);
            } else {
                let parent_block_info: Block<Transaction> =
                    serde_json::from_value(parent_block_info.unwrap())?;
                self.block_queue.push_front(parent_block_info);
            }
        }

        while let Some(block_info) = self.block_queue.pop_front() {
            let diff = self
                .rpc_client
                .block_diff(BlockId::Hash(block_info.hash.unwrap()))
                .await?;
            let mut bytes = diff.as_ref();
            let block_storage_diff = BlockStorageDiff::decode(&mut bytes)?;
            self.snaps.update_block(block_info, block_storage_diff)?;
        }

        Ok(true)
    }

    pub fn start(mut self) -> watch::Sender<()> {
        let (tx, mut rx) = watch::channel(());
        let mut interval = interval(std::time::Duration::from_secs(5));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        break;
                    }
                    _ = interval.tick() => {
                        loop {
                            let res = self.update().await;
                            if let Err(e) = res {
                                error!(target:"updater", "update error: {}", e);
                                break;
                            }
                            if !res.unwrap() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        tx
    }
}
