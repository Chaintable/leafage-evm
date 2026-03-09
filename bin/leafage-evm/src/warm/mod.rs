use crate::utils::{s3_get_block_transactions_by_number, KafkaS3Config};
use anyhow::{Context, Result};
use aws_sdk_s3::Client;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};
use leafage_evm_rpc::{ApiBuilder, TokenCollector};
use leafage_evm_storage::{BlockIndex, EvmStorageRead, EvmStorageWrite};
use leafage_evm_types::{Address, DebankTransaction};
use tokio::task::JoinSet;
use tracing::{error, info};

pub struct Warmup<Tree> {
    rpc_client: Option<HttpClient>,
    s3_client: Client,
    kafka_s3_cfg: KafkaS3Config,
    tree: Tree,
    warmup_blocks: usize,
    warmup_tokens: usize,
    init_task_queue_size: usize,
    token_collector: Option<TokenCollector>,
}

impl<Tree> Warmup<Tree>
where
    Tree: EvmStorageRead
        + EvmStorageWrite<Error = <Tree as EvmStorageRead>::Error>
        + Send
        + Sync
        + 'static,
{
    pub async fn new(
        rpc_url: Option<impl AsRef<str>>,
        kafka_s3_cfg: KafkaS3Config,
        tree: Tree,
        warmup_blocks: usize,
        warmup_tokens: usize,
        init_task_queue_size: usize,
        token_collector: Option<TokenCollector>,
    ) -> Result<Self> {
        let mut rpc_client = None;
        if let Some(rpc_url) = rpc_url {
            let client = HttpClientBuilder::default().build(rpc_url.as_ref())?;
            rpc_client = Some(client);
        }
        let s3_config = aws_config::load_from_env().await;
        let s3_client = Client::new(&s3_config);
        Ok(Self {
            rpc_client,
            s3_client,
            kafka_s3_cfg,
            tree,
            warmup_blocks,
            warmup_tokens,
            init_task_queue_size,
            token_collector,
        })
    }

    // only for replay block
    async fn fetch_warmup_blocks(&self) -> anyhow::Result<Vec<Vec<DebankTransaction>>> {
        let mut res = Vec::with_capacity(self.warmup_blocks);
        let end_block_number = self.tree.last_committed_block()?.unwrap().header.number;
        let start_block_number = end_block_number
            .checked_sub(self.warmup_blocks as u64 - 1)
            .unwrap_or_default();

        let batch_size = self.init_task_queue_size as u64;
        let mut current_start_block = start_block_number;
        while current_start_block <= end_block_number {
            let mut fetch_transactions_join_set = JoinSet::new();
            let current_end_block =
                std::cmp::min(current_start_block + batch_size - 1, end_block_number);
            for block_num in current_start_block..=current_end_block {
                let rpc_client = self.rpc_client.clone();
                let s3_client = self.s3_client.clone();
                let outer_bucket_name = self.kafka_s3_cfg.outer_bucket_name.clone();
                let s3_chain_id = self.kafka_s3_cfg.s3_chain_id.clone();
                let version = self.kafka_s3_cfg.version.clone();
                fetch_transactions_join_set.spawn(async move {
                    s3_get_block_transactions_by_number(
                        &rpc_client,
                        &s3_client,
                        &outer_bucket_name,
                        &s3_chain_id,
                        &version,
                        block_num,
                    )
                    .await
                    .context(format!("s3 get transactions failed, {block_num}"))
                });
            }
            for transactions in fetch_transactions_join_set.join_all().await {
                let transactions = transactions?;
                res.push(transactions);
            }
            current_start_block += batch_size;
        }
        info!(target: "updater", "Fetch {} warmup blocks", res.len());
        Ok(res)
    }

    pub async fn with_warmup_data<DB>(&self, mut builder: ApiBuilder<DB>) -> ApiBuilder<DB>
    where
        DB: EvmStorageRead + BlockIndex + Sync + Send + 'static,
    {
        if self.warmup_blocks > 0 {
            let blocks = match self.fetch_warmup_blocks().await {
                Ok(blocks) => blocks,
                Err(err) => {
                    error!(target:"updater", "failed to fetch warmup blocks: {}", err);
                    return builder;
                }
            };
            builder = builder.with_replay_blocks(blocks);
        }
        if self.warmup_tokens > 0 {
            let owner = Address::random();
            let mut tokens = Vec::new();
            if let Some(ref collector) = self.token_collector {
                tokens = collector.get_all().await.into_iter().take(self.warmup_tokens).collect();
                info!(
                    target: "updater", "fetch local collected tokens, total unique warmup tokens: {}", tokens.len()
                );
            }

            builder = builder.with_warmup_erc20_addresses(owner, tokens);
        }
        builder
    }
}
