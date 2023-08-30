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
use tracing::{debug, error, info};

/// [`Updater`] is used to update the snapshot tree to the latest block
pub struct Updater<DB> {
    rpc_client: HttpClient,
    snap_tree: Arc<SnapshotTree<DB>>,
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
    pub fn new(snap_tree: Arc<SnapshotTree<DB>>, rpc_url: impl AsRef<str>) -> Result<Self> {
        let rpc_client = HttpClientBuilder::default().build(rpc_url)?;
        Ok(Self {
            rpc_client,
            snap_tree,
            block_queue: VecDeque::new(),
        })
    }

    /// Update the snapshot tree to the latest block
    /// Return true if the snapshot tree is updated
    async fn update(&mut self) -> Result<bool> {
        if self.block_queue.is_empty() {
            let current_block_info = self.snap_tree.block_info()?;
            let next_block_number =
                BlockNumber::Number((current_block_info.number.unwrap().as_u64() + 1).into());
            let next_block_info = self
                .rpc_client
                .get_block_by_number(next_block_number, true)
                .await;
            info!(target:"updater", "current block number {:?}", current_block_info.number);
            let next_block_info = next_block_info?;
            if next_block_info.is_none() {
                info!(target:"updater", "no new block");
                return Ok(false);
            } else {
                let next_block_info: Block<Transaction> =
                    serde_json::from_value(next_block_info.unwrap())?;
                self.block_queue.push_back(next_block_info);
            }
        }
        // find the first block whose parent block is in the snapshot tree
        loop {
            if self.block_queue.len() > 256 {
                info!(target:"updater", "can't find parent block in 256 blocks");
                return Ok(false);
            }
            let first_block_info = self.block_queue.front().unwrap();
            if self
                .snap_tree
                .state_at(BlockId::Hash(first_block_info.parent_hash))?
                .is_some()
            {
                debug!(target:"updater", "find parent block {}", first_block_info.parent_hash);
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
                .block_diff(BlockId::Hash(block_info.hash.unwrap()), true)
                .await?;
            let mut bytes = diff.as_ref();
            let block_storage_diff = BlockStorageDiff::decode(&mut bytes)?;
            let block_hash = block_info.hash.unwrap();
            let block_number = block_info.number.unwrap();
            self.snap_tree
                .update_block(block_info, block_storage_diff)?;
            info!(target:"updater", "update block hash {}, block num {}", block_hash, block_number);
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
                        info!(target:"updater", "stop updater");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_fetch_block_diff() {
        let rpc_client = HttpClientBuilder::default()
            .build("http://127.0.0.1:3545")
            .unwrap();
        for i in 0..100 {
            let res = rpc_client
                .block_diff(
                    BlockId::Number(BlockNumber::Number((18017515 + i).into())),
                    true,
                )
                .await
                .unwrap();
            let block_diff: BlockStorageDiff = Decodable::decode(&mut res.as_ref()).unwrap();
            let res = rpc_client
                .get_block_by_number(BlockNumber::Number((18017515 + i).into()), false)
                .await
                .unwrap();
            if !block_diff.new_codes.is_empty() {
                println!("{:?}  new code, res {:?}", 18017515 + i, res.is_some());
            } else {
                println!("{:?} no new code, res {:?}", 18017515 + i, res.is_some());
            }
        }
    }
}
