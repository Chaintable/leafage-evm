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
        if let Some(rpc_address) = rpc_url.clone() {
            let mut initializer = HttpInitializer::new(latest_db, rpc_address)?;
            initializer.init().await?;
        } else if let Some(kafka_s3_config) = kafka_s3_cfg {
            let mut initializer =
                KafkaInitializer::new(latest_db, kafka_s3_config, genesis_number).await?;
            initializer.init().await?;
        }
    }
    Ok(())
}
