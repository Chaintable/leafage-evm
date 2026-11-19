//! Parent-state Cosmos parameters used by DeliverTx. Querying the completed
//! block would also include any governance changes during its EndBlock.
use super::chain_config::EvmParams;
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use jsonrpsee::{core::client::ClientT, http_client::HttpClient, rpc_params};
use leafage_evm_rpc::{CronosEra, CronosExecutionParams};
use prost::Message;
use serde::Deserialize;

// ethermint/{feemarket,evm}/v1/query.proto QueryParamsResponse.params
#[derive(Clone, PartialEq, Message)]
struct QueryParamsResponse {
    #[prost(bytes = "vec", optional, tag = "1")]
    params: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct FeeMarketParams {
    // cosmos-sdk LegacyDec protobuf customtype encodes its scaled integer as ASCII.
    #[prost(string, tag = "8")]
    min_gas_multiplier: String,
}

#[derive(Deserialize)]
struct QueryResult {
    response: AbciResponse,
}

#[derive(Deserialize)]
struct AbciResponse {
    code: u32,
    log: String,
    height: String,
    value: Option<String>,
}

/// The Comet node could not serve the requested historical query.
#[derive(Debug)]
pub(super) struct HistoricalParamsUnavailable;

impl std::fmt::Display for HistoricalParamsUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("historical Ethermint parameters unavailable")
    }
}

async fn query<M: Message + Default>(comet: &HttpClient, path: &str, height: u64) -> Result<M> {
    let result: QueryResult = comet
        .request(
            "abci_query",
            rpc_params![path, "", height.to_string(), false],
        )
        .await
        .with_context(|| format!("querying {path} at height {height}"))?;
    let response = result.response;
    if response.code != 0 {
        return Err(anyhow::anyhow!(
            "{path} ABCI code {} at {height}: {}",
            response.code,
            response.log
        )
        .context(HistoricalParamsUnavailable));
    }
    ensure!(
        response.height.parse::<u64>()? == height,
        "{path} height mismatch at {height}"
    );
    let bytes = STANDARD.decode(response.value.context("missing ABCI query value")?)?;
    let response = QueryParamsResponse::decode(bytes.as_slice())?;
    let params = response.params.context("missing Ethermint parameters")?;
    Ok(M::decode(params.as_slice())?)
}

pub(super) async fn at_block(
    comet: &HttpClient,
    height: u64,
    era: CronosEra,
) -> Result<CronosExecutionParams> {
    if matches!(
        era,
        CronosEra::BeforeHuygen | CronosEra::Huygen | CronosEra::HuygenHotfix
    ) {
        return Ok(CronosExecutionParams::default());
    }
    let parent = height.checked_sub(1).context("missing parent height")?;
    let min_gas_multiplier_atomics =
        if era == CronosEra::Galileo && CronosEra::at_height(parent) != Some(era) {
            // v0.20.1-cronos x/feemarket/migrations/v011/migrate.go installs 0.5
            // in the upgrade handler before the first v1 transaction.
            500_000_000_000_000_000
        } else {
            let params: FeeMarketParams =
                query(comet, "/ethermint.feemarket.v1.Query/Params", parent).await?;
            let multiplier = params
                .min_gas_multiplier
                .parse::<u64>()
                .context("invalid MinGasMultiplier")?;
            ensure!(
                multiplier <= 1_000_000_000_000_000_000,
                "MinGasMultiplier exceeds one"
            );
            multiplier
        };
    let evm_params: EvmParams = query(comet, "/ethermint.evm.v1.Query/Params", parent).await?;
    evm_params.validate(height)?;
    let history_serve_window = if era == CronosEra::V17 {
        evm_params.history_serve_window
    } else {
        0
    };
    Ok(CronosExecutionParams {
        min_gas_multiplier_atomics,
        history_serve_window,
    })
}
