mod http_initializer;
pub use http_initializer::Initializer as HttpInitializer;

mod kafka_initializer;
pub use kafka_initializer::Initializer as KafkaInitializer;

use crate::utils::KafkaS3Config;
use anyhow::{Ok, Result};
use leafage_evm_storage::{ArchiveDBProvider, EvmStorageWrite, StateDBWrapper};
use leafage_evm_types::{BlockId, BlockNumberOrTag};

pub async fn initialize_check<DB: ArchiveDBProvider + Send + Sync + 'static>(
    db: DB,
    rpc_url: Option<String>,
    kafka_s3_cfg: Option<KafkaS3Config>,
    genesis_number: u64,
) -> Result<()> {
    let db = db
        .db_at(BlockId::Number(BlockNumberOrTag::Latest))?
        .unwrap();
    let latest_db = StateDBWrapper(db);
    if latest_db.last_committed_block()?.is_none() {
        match (rpc_url, kafka_s3_cfg) {
            (Some(rpc_url), None) => {
                let mut initializer = HttpInitializer::new(latest_db, rpc_url)?;
                initializer.init().await?;
            }
            (rpc_url, Some(kafka_s3_cfg)) => {
                let mut initializer =
                    KafkaInitializer::new(latest_db, rpc_url, kafka_s3_cfg, genesis_number).await?;
                initializer.init().await?;
            }
            (None, None) => {
                anyhow::bail!("The database is uninitialized, please provide rpc_url or kafka_s3_cfg to initialize the database.");
            }
        }
    }
    Ok(())
}
