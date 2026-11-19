mod chain_config;
mod messages;
mod params;

use alloy::{
    consensus::transaction::SignerRecoverable,
    eips::eip2718::Encodable2718,
    primitives::{keccak256, B256},
    rpc::types::{Block, BlockTransactions},
};
use anyhow::{ensure, Context, Result};
use jsonrpsee::{
    core::client::ClientT,
    http_client::{HttpClient, HttpClientBuilder},
    rpc_params,
};
use leafage_evm_rpc::{CronosArchiveRead, CronosEra, CronosRelease};
use leafage_evm_storage::{
    BlockIndex, EvmStorageRead, MultiStorage, StateTree, StateTreeConfig, StorageKind,
};
use leafage_evm_types::{BlockId, BlockNumberOrTag};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, clap::Parser)]
pub struct Command {
    #[arg(long, value_parser = ["cronos"])]
    chain: String,
    /// Offline Leafage archive directory containing RocksDB CURRENT.
    #[arg(long)]
    db_path: PathBuf,
    #[arg(long)]
    evm_rpc: String,
    /// Comet archive RPC serving blocks, results, consensus parameters and historical ABCI queries.
    #[arg(long)]
    comet_rpc: String,
    /// First mainnet height in the inclusive range.
    #[arg(long)]
    from_block: u64,
    #[arg(long)]
    to_block: u64,
    /// Directory for BlockFiles, per-height diagnostics and the range report.
    #[arg(long)]
    output_dir: PathBuf,
    #[arg(long)]
    archive_zstd_compression: bool,
    #[arg(long)]
    inverted_block_encoding: bool,
}

fn publish(path: &Path, value: &Value) -> Result<()> {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_vec(value)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn tx_array(value: Option<&Value>) -> Result<&[Value]> {
    match value {
        Some(Value::Array(values)) => Ok(values),
        Some(Value::Null) => Ok(&[]),
        _ => anyhow::bail!("missing transaction list"),
    }
}

fn comet_hash(value: &Value) -> Result<B256> {
    Ok(value.as_str().context("missing Comet hash")?.parse()?)
}

fn rule(height: u64, empty: bool) -> &'static str {
    if height == 1 {
        "genesis-native-token-v1"
    } else if empty {
        "native-v2"
    } else {
        CronosEra::at_height(height).map_or("uncovered", CronosEra::version)
    }
}

fn cached_bytes(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl Command {
    pub async fn run(self) -> Result<()> {
        ensure!(
            self.from_block > 0 && self.from_block <= self.to_block,
            "expected a nonempty range starting after genesis"
        );
        fs::create_dir_all(&self.output_dir)?;
        let summary = self
            .output_dir
            .join(format!("{}-{}.json", self.from_block, self.to_block));
        publish(
            &summary,
            &json!({"from":self.from_block,"to":self.to_block,"backfill_ready":false,"status":"incomplete"}),
        )?;
        let counts = match self.run_range().await {
            Ok(counts) => counts,
            Err(err) => {
                publish(
                    &summary,
                    &json!({"from":self.from_block,"to":self.to_block,
                    "status":"failed","backfill_ready":false,"detail":format!("{err:#}")}),
                )
                .with_context(|| format!("could not record replay failure: {err:#}"))?;
                return Err(err);
            }
        };
        let ready = counts.len() == 1 && counts.contains_key("verified");
        publish(
            &summary,
            &json!({"from":self.from_block,"to":self.to_block,"status":"complete","counts":counts,"backfill_ready":ready}),
        )?;
        ensure!(ready, "requested range contains unverified blocks");
        Ok(())
    }

    async fn run_range(&self) -> Result<BTreeMap<String, u64>> {
        ensure!(
            self.db_path.join("CURRENT").is_file(),
            "archive database missing"
        );
        let evm = HttpClientBuilder::default().build(&self.evm_rpc)?;
        let comet = HttpClientBuilder::default().build(&self.comet_rpc)?;
        let chain_id: String = evm.request("eth_chainId", rpc_params![]).await?;
        ensure!(
            chain_id == "0x19",
            "expected Cronos mainnet, got {chain_id}"
        );
        leafage_evm_storage::set_inverted_block_encoding(self.inverted_block_encoding);
        let db = MultiStorage::open(
            &self.db_path,
            512,
            StorageKind::Rocksdb,
            true,
            false,
            self.archive_zstd_compression,
        )?;
        let tree = StateTree::new(db, StateTreeConfig::new(128, 10000, 100000, 10000, true))?;
        let mut counts = BTreeMap::new();
        for height in self.from_block..=self.to_block {
            let mut report = match self.block(&evm, &comet, &tree, height).await {
                Ok(report) => report,
                Err(err) if err.is::<CronosArchiveRead>() || err.is::<std::io::Error>() => {
                    return Err(err.context(format!("replaying block {height}")));
                }
                Err(err) if err.is::<chain_config::UnauditedParams>() => {
                    json!({"height":height, "status":"rules_uncovered","detail":format!("{err:#}")})
                }
                Err(err) if err.is::<params::HistoricalParamsUnavailable>() => {
                    json!({"height":height, "status":"input_gap","detail":format!("{err:#}")})
                }
                Err(err) => {
                    let status = match err.downcast_ref::<jsonrpsee::core::ClientError>() {
                        Some(jsonrpsee::core::ClientError::ParseError(_)) | None => "invalid_input",
                        Some(_) => "input_gap",
                    };
                    json!({"height":height, "status":status, "detail":format!("{err:#}")})
                }
            };
            report["release"] = json!(CronosRelease::at_height(height));
            *counts
                .entry(
                    report["status"]
                        .as_str()
                        .context("missing status")?
                        .to_owned(),
                )
                .or_insert(0u64) += 1;
            publish(
                &self.output_dir.join(format!("{height}.report.json")),
                &report,
            )?;
        }
        Ok(counts)
    }

    fn completed(
        &self,
        height: u64,
        hash: B256,
        parent: B256,
        tree: &StateTree<MultiStorage>,
        version: &str,
    ) -> Result<Option<Value>> {
        let path = self.output_dir.join(format!("{height}.report.json"));
        let Some(report) =
            cached_bytes(&path)?.and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        else {
            return Ok(None);
        };
        let name = format!("{height}-{hash}-{version}.json");
        if report["status"] != "verified"
            || report["hash"] != json!(hash)
            || report["parent"] != json!(parent)
            || report["block_file"] != name
            || report["release"] != json!(CronosRelease::at_height(height))
            || report["rule"] != version
        {
            return Ok(None);
        }
        let Some(info) = tree
            .get_block_by_id_arc(BlockId::Number(BlockNumberOrTag::Number(height)))
            .context(CronosArchiveRead)?
        else {
            return Ok(None);
        };
        if info.header.hash != hash || info.header.parent_hash != parent {
            return Ok(None);
        }
        let Some(bytes) = cached_bytes(&self.output_dir.join(name))? else {
            return Ok(None);
        };
        Ok(
            (report["artifact_digest"] == json!(alloy::primitives::keccak256(bytes)))
                .then_some(report),
        )
    }

    async fn block(
        &self,
        evm: &HttpClient,
        comet: &HttpClient,
        tree: &StateTree<MultiStorage>,
        height: u64,
    ) -> Result<Value> {
        let block: Option<Block> = evm
            .request(
                "eth_getBlockByNumber",
                rpc_params![format!("{height:#x}"), true],
            )
            .await?;
        let Some(block) = block else {
            return Ok(json!({"height":height,"status":"input_gap","detail":"EVM block missing"}));
        };
        ensure!(block.header.number == height, "EVM block height mismatch");
        if let Some(era) = CronosEra::at_height(height) {
            era.spec(block.header.base_fee_per_gas)?;
        }
        let hash = block.header.hash;
        let parent = block.header.parent_hash;
        if let Some(report) = self.completed(
            height,
            hash,
            parent,
            tree,
            rule(height, block.transactions.is_empty()),
        )? {
            return Ok(report);
        }
        let tm: Value = comet
            .request("block", rpc_params![height.to_string()])
            .await?;
        let results: Value = comet
            .request("block_results", rpc_params![height.to_string()])
            .await?;
        let params: Value = comet
            .request("consensus_params", rpc_params![height.to_string()])
            .await?;
        for h in [
            &tm["block"]["header"]["height"],
            &results["height"],
            &params["block_height"],
        ] {
            ensure!(
                h.as_str() == Some(height.to_string().as_str()),
                "Comet height mismatch"
            );
        }
        let raw_parent = &tm["block"]["header"]["last_block_id"]["hash"];
        let comet_parent = if height == 1 && raw_parent.as_str() == Some("") {
            B256::ZERO
        } else {
            comet_hash(raw_parent)?
        };
        ensure!(
            comet_hash(&tm["block_id"]["hash"])? == hash && comet_parent == parent,
            "Comet block hash mismatch"
        );
        let max_gas = params["consensus_params"]["block"]["max_gas"]
            .as_str()
            .context("missing max_gas")?
            .parse::<i64>()?;
        ensure!(max_gas >= -1, "invalid Comet max_gas {max_gas}");
        let hashes: Vec<_> = block.transactions.hashes().collect();
        if !hashes.is_empty() {
            let BlockTransactions::Full(txs) = &block.transactions else {
                anyhow::bail!("missing signed bodies")
            };
            for (i, tx) in txs.iter().enumerate() {
                let envelope = tx.inner.inner();
                ensure!(
                    keccak256(envelope.encoded_2718()) == hashes[i]
                        && envelope.recover_signer()? == tx.inner.signer()
                        && tx.block_hash == Some(hash)
                        && tx.transaction_index == Some(i as u64),
                    "signed transaction identity differs at index {i}"
                );
            }
        }
        let mut receipts = Vec::new();
        for (index, tx) in hashes.iter().enumerate() {
            let receipt: Option<alloy::rpc::types::TransactionReceipt> = evm
                .request("eth_getTransactionReceipt", rpc_params![tx])
                .await?;
            let Some(receipt) = receipt else {
                return Ok(
                    json!({"height":height,"status":"input_gap","detail":format!("receipt missing for {tx}")}),
                );
            };
            ensure!(
                receipt.block_hash == Some(hash)
                    && receipt.block_number == Some(height)
                    && receipt.transaction_index == Some(index as u64)
                    && receipt.transaction_hash == *tx,
                "receipt identity mismatch at {index}"
            );
            receipts.push(receipt);
        }
        let mapping = messages::positions(
            tx_array(tm.pointer("/block/data/txs"))?,
            tx_array(results.get("txs_results"))?,
            &hashes,
        )?;
        let mut report = json!({"height":height,"hash":hash,"parent":parent,"max_gas":max_gas,"messages":mapping});
        let native_prefix = mapping
            .iter()
            .rposition(|m| m.evm_tx.is_some())
            .map(|last| {
                mapping[..last]
                    .iter()
                    .filter(|m| m.evm_tx.is_none())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let version = rule(height, hashes.is_empty());
        report["rule"] = json!(version);
        if !native_prefix.is_empty() {
            report["status"] = json!("native_state_gap");
            report["preceding_messages"] = json!(native_prefix);
            report["detail"] = json!(
                "archive exposes block boundaries; intermediate Cosmos and ante state unavailable"
            );
            report["cosmos_results"] = results["txs_results"].clone();
            return Ok(report);
        }
        if version == "uncovered" {
            report["status"] = json!("rules_uncovered");
            return Ok(report);
        }
        let Some(info) = tree
            .get_block_by_id_arc(BlockId::Number(BlockNumberOrTag::Number(height)))
            .context(CronosArchiveRead)?
        else {
            report["status"] = json!("archive_gap");
            report["detail"] = json!("archive block missing");
            return Ok(report);
        };
        ensure!(
            info.header.hash == hash && info.header.parent_hash == parent,
            "archive block identity mismatch"
        );
        let mut file = if height == 1 {
            ensure!(
                hashes.is_empty() && parent == B256::ZERO,
                "unexpected Cronos genesis block"
            );
            leafage_evm_rpc::native_token_genesis_block(&block)
        } else if hashes.is_empty() {
            leafage_evm_rpc::native_block_file(&block.header)
        } else {
            report["receipts"] = serde_json::to_value(&receipts)?;
            let mut message_gas = Vec::new();
            for m in mapping.iter().filter(|m| m.evm_tx.is_some()) {
                let result = &results["txs_results"][m.cosmos_tx];
                if mapping
                    .iter()
                    .any(|other| other.cosmos_tx == m.cosmos_tx && other.msg != m.msg)
                {
                    report["status"] = json!("native_state_gap");
                    report["detail"] = json!(format!(
                        "multi-message ante and fee state at Cosmos tx {}",
                        m.cosmos_tx
                    ));
                    return Ok(report);
                }
                message_gas.push(
                    result["gas_used"]
                        .as_str()
                        .context("missing Cosmos execution gas")?
                        .parse()?,
                );
            }
            let Some(state) = tree
                .state_at(BlockId::Hash(parent.into()))
                .context(CronosArchiveRead)?
            else {
                report["status"] = json!("archive_gap");
                report["detail"] = json!("archive parent state missing");
                return Ok(report);
            };
            let era = CronosEra::at_height(height).context("missing replay rule")?;
            let parameters = params::at_block(comet, height, era).await?;
            match leafage_evm_rpc::replay_cronos_ethermint(
                leafage_evm_storage::EvmStorageWrapper::new(state),
                &block,
                &receipts,
                max_gas.max(0) as u64,
                &message_gas,
                &parameters,
                &messages::begin_transfers(&results)?,
            ) {
                Ok(file) => file,
                Err(err) if err.is::<CronosArchiveRead>() => return Err(err),
                Err(err) => {
                    report["status"] = if err.is::<leafage_evm_rpc::CronosNativeCall>() {
                        json!("native_state_gap")
                    } else if err.is::<leafage_evm_rpc::CronosRuleGap>()
                        || err.is::<leafage_evm_rpc::LegacySelfdestruct>()
                    {
                        json!("rules_uncovered")
                    } else {
                        json!("execution_mismatch")
                    };
                    report["evm_block"] = serde_json::to_value(&block)?;
                    report["comet_block"] = tm;
                    report["comet_results"] = results;
                    report["consensus_params"] = params;
                    report["detail"] = json!(format!("{err:#}"));
                    return Ok(report);
                }
            }
        };
        if height > 1 {
            messages::extend_storage_contracts(&mut file.storage_contracts, &block, &results)?;
        }
        let name = format!("{height}-{hash}-{version}.json");
        let value = serde_json::to_value(&file)?;
        report["artifact_digest"] = json!(keccak256(serde_json::to_vec(&value)?));
        publish(&self.output_dir.join(&name), &value)?;
        report["block_file"] = json!(name);
        report["validation_hash"] = json!(file.validation_hash());
        report["status"] = json!("verified");
        Ok(report)
    }
}
