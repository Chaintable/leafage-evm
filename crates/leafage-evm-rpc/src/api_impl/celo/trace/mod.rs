//! Celo L1 pipeline outputs from one local replay on archive state.
mod fees;
mod finalize;
mod genesis;
mod input;
mod replay;

use crate::{
    api_impl::core::{Api, ApiBase},
    error::{invalid_params_rpc_err, rpc_error_with_code},
    pipeline::{
        chain::ReplayOptions,
        rpc::{TraceBlockOptions, TraceDebankApiServer},
        source::ReplayDataSource,
        types::DebankBlockOutput,
    },
};
use jsonrpsee::core::{async_trait, RpcResult};
use leafage_evm_storage::{BlockIndex, EvmStorageRead};
use leafage_evm_types::{BlockId, DebankErrorCode};

pub(crate) struct CeloTrace<C> {
    pub api: Api<C>,
    pub source: ReplayDataSource,
    pub check_state: bool,
}

fn error(err: impl std::fmt::Display) -> jsonrpsee::types::ErrorObjectOwned {
    rpc_error_with_code(DebankErrorCode::EvmFailed as i32, err.to_string())
}

#[async_trait]
impl<C> TraceDebankApiServer for CeloTrace<C>
where
    C: ApiBase,
    C::DB: EvmStorageRead + BlockIndex + Clone + Send + Sync + 'static,
{
    async fn debank_block(
        &self,
        id: BlockId,
        options: Option<TraceBlockOptions>,
    ) -> RpcResult<DebankBlockOutput> {
        let options = ReplayOptions {
            strict: true,
            check_state: self.check_state,
            state_diff: options.unwrap_or_default().state_diff,
            traces: true,
        };
        let started = std::time::Instant::now();
        let db = self.api.inner.db().clone();
        let info = db.get_block_by_id_arc(id).map_err(error)?.ok_or_else(|| {
            rpc_error_with_code(
                DebankErrorCode::BlockNotFound as i32,
                format!("block {id:?} is not in the archive"),
            )
        })?;
        let number = info.header.number;
        leafage_evm_chains::celo::l1::L1Fork::at(number).map_err(invalid_params_rpc_err)?;
        let parent = if number == 0 {
            None
        } else {
            Some(
                db.get_block_by_id_arc(BlockId::Hash(info.header.parent_hash.into()))
                    .map_err(error)?
                    .ok_or_else(|| error("parent block is not in the archive"))?,
            )
        };
        let permit = match self.api.inner.evm_cfg().exec_limiter.as_ref() {
            Some(limit) => Some(limit.clone().acquire_owned().await.map_err(error)?),
            None => None,
        };
        let data = self
            .source
            .fetch(info.header.hash)
            .await
            .map_err(|e| error(format!("replay input: {e:#}")))?;
        let fetched = started.elapsed();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let input = input::BlockInput::verified(data, &info.header)?;
            replay::replay(&db, &info, parent.as_deref(), input, options)
        })
        .await
        .map_err(error)?
        .map_err(|e| error(format!("{e:#}")))?;
        tracing::info!(target: "trace", number, txs = result.block_file.txs.len(),
            fetch_ms = fetched.as_millis() as u64, total_ms = started.elapsed().as_millis() as u64,
            "Celo L1 trace_debankBlock");
        Ok(result)
    }
}
