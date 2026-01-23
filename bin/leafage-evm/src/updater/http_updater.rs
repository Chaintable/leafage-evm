use alloy_rlp::Decodable;
use anyhow::Result;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::{EthApiClient, TraceApiClient};
use leafage_evm_storage::{BlockContext, EvmStorageRead, EvmStorageWrite};
use leafage_evm_types::{Block, BlockId, BlockNumberOrTag, BlockStorageDiff, DebankOutPut};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::{debug, error, info};

/// [`Updater`] is used to update the snapshot tree to the latest block
pub struct Updater<Tree> {
    rpc_client: HttpClient,
    tree: Tree,
    block_queue: VecDeque<DebankOutPut>,
    update_interval: Duration,
    max_diff_depth: usize,
}

impl<Tree> Updater<Tree>
where
    Tree: EvmStorageRead
        + EvmStorageWrite<Error = <Tree as EvmStorageRead>::Error>
        + Send
        + Sync
        + 'static,
{
    pub fn new(
        snap_tree: Tree,
        rpc_url: impl AsRef<str>,
        update_interval: Duration,
        max_diff_depth: usize,
    ) -> Result<Self> {
        let rpc_client = HttpClientBuilder::default().build(rpc_url)?;
        Ok(Self {
            rpc_client,
            tree: snap_tree,
            block_queue: VecDeque::new(),
            update_interval,
            max_diff_depth,
        })
    }

    /// Update the snapshot tree to the latest block
    /// Return true if the snapshot tree is updated
    async fn update(&mut self) -> Result<bool> {
        if self.block_queue.is_empty() {
            let current_block_info = self
                .tree
                .state_at(BlockId::latest())?
                .unwrap()
                .block_info_arc()?;
            let latest_block_num = self.rpc_client.block_number().await?;
            let latest_block_num: u64 = latest_block_num.try_into()?;
            if latest_block_num <= current_block_info.header.number {
                debug!(target:"updater", "no new block");
                return Ok(false);
            }
            let next_block_number =
                BlockNumberOrTag::Number((current_block_info.header.number + 1).into());
            let debank_output = self
                .rpc_client
                .debank_block(BlockId::Number(next_block_number))
                .await?;
            info!(target:"updater", "current block number {:?}", current_block_info.header.number);
            self.block_queue.push_back(debank_output);
        }
        // find the first block whose parent block is in the snapshot tree
        loop {
            if self.block_queue.len() > self.max_diff_depth {
                info!(target:"updater", "can't find parent block before max diff depth, drop");
                return Ok(false);
            }
            let first_output = self.block_queue.front().unwrap();
            if self
                .tree
                .state_at(BlockId::Hash(first_output.header.parent_hash.into()))?
                .is_some()
            {
                debug!(target:"updater", "find parent block {}", first_output.header.parent_hash);
                break;
            }
            let debank_output = self
                .rpc_client
                .debank_block(BlockId::Hash(first_output.header.parent_hash.into()))
                .await?;
            self.block_queue.push_front(debank_output);
        }

        while let Some(debank_output) = self.block_queue.pop_front() {
            let block_storage_diff = if debank_output.state_diff.is_empty() {
                BlockStorageDiff::default()
            } else {
                let mut bytes = debank_output.state_diff.as_ref();
                BlockStorageDiff::decode(&mut bytes)?
            };
            let block_hash = debank_output.header.hash;
            let block_num = debank_output.header.number;
            let new_accounts_num = block_storage_diff.new_accounts.len();
            let deleted_accounts_num = block_storage_diff.deleted_accounts.len();
            let new_codes_num = block_storage_diff.new_codes.len();
            let block_info = Block::empty(debank_output.header);
            self.tree.update_block(block_info, block_storage_diff)?;
            info!(target:"updater", "update block hash {}, block num {}, new accounts num {}, deleted accounts num {}, new codes num {}",
                                            block_hash, block_num, new_accounts_num, deleted_accounts_num, new_codes_num);
        }

        Ok(true)
    }

    pub fn start(mut self) -> watch::Sender<()> {
        let (tx, mut rx) = watch::channel(());
        let mut interval = interval(self.update_interval);
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
        for i in 0..1 {
            let res = rpc_client
                .debank_block(BlockId::Number(BlockNumberOrTag::Number(18022783 + i)))
                .await
                .unwrap();
            let block_diff: BlockStorageDiff = Decodable::decode(&mut res.state_diff.as_ref()).unwrap();
            println!("{:?}", block_diff.storage_diffs);
        }
    }
}
