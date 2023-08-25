use anyhow::{bail, Result};
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::LeafAgeApiClient;
use leafage_evm_storage::{BlockContext, EvmStorageRead, EvmStorageWrite, SnapshotTree, StateDB};
use leafage_evm_types::{BlockId, BlockNumber, BlockStorageDiff};
use open_fastrlp::Decodable;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::interval;
use tracing::error;

pub struct Updater<DB> {
    rpc_client: HttpClient,
    snaps: Arc<SnapshotTree<DB>>,
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
        Ok(Self { rpc_client, snaps })
    }

    async fn update(&self) -> Result<()> {
        let current_block_info = self.snaps.block_info()?;
        let next_block_number =
            BlockNumber::Number((current_block_info.number.as_u64() + 1).into());
        let mut next_block_infos = self
            .rpc_client
            .block_info(BlockId::Number(next_block_number), 1)
            .await?;
        let mut block_stack = VecDeque::default();
        block_stack.push_back(next_block_infos[0].clone());
        if !self
            .snaps
            .state_at(BlockId::Hash(next_block_infos[0].parent_hash))?
            .is_some()
        {
            next_block_infos = self
                .rpc_client
                .block_info(
                    BlockId::Number(BlockNumber::Number(
                        (next_block_infos[0].number.as_u64() - 128).into(),
                    )),
                    128,
                )
                .await?;
            if !self
                .snaps
                .state_at(BlockId::Hash(next_block_infos[0].parent_hash))?
                .is_some()
            {
                bail!("too many blocks to rollback");
            }
            next_block_infos.reverse();
            for block_info in next_block_infos {
                if self
                    .snaps
                    .state_at(BlockId::Hash(block_info.parent_hash))?
                    .is_some()
                {
                    break;
                } else {
                    block_stack.push_front(block_info.clone());
                }
            }
        }
        for block_info in block_stack {
            let diff = self
                .rpc_client
                .block_diff(BlockId::Hash(block_info.hash))
                .await?;
            let mut bytes = diff.as_ref();
            let block_storage_diff = BlockStorageDiff::decode(&mut bytes)?;
            self.snaps.update_block(block_info, block_storage_diff)?;
        }

        Ok(())
    }

    pub fn start(self) -> watch::Sender<()> {
        let (tx, mut rx) = watch::channel(());
        let mut interval = interval(std::time::Duration::from_secs(5));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = rx.changed() => {
                        break;
                    }
                    _ = interval.tick() => {
                        if let Err(e) = self.update().await {
                            error!(target:"updater", "update error: {}", e);
                        }
                    }
                }
            }
        });
        tx
    }
}
