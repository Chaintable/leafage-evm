mod messages;

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
    /// Comet RPC serving block, block_results and consensus_params at the requested heights.
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
    if empty {
        return "native-v1";
    }
    match height {
        1..2_693_800 => "ethermint-transient-v1",
        2_693_800..6_542_800 => "ethermint-statedb-v1",
        _ => "uncovered",
    }
}

impl Command {
    pub async fn run(self) -> Result<()> {
        ensure!(
            self.from_block > 0 && self.from_block <= self.to_block,
            "expected a nonempty range starting after genesis"
        );
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
        fs::create_dir_all(&self.output_dir)?;
        let summary = self
            .output_dir
            .join(format!("{}-{}.json", self.from_block, self.to_block));
        publish(
            &summary,
            &json!({"from":self.from_block,"to":self.to_block,"backfill_ready":false,"status":"running"}),
        )?;
        let mut counts = BTreeMap::new();
        for height in self.from_block..=self.to_block {
            let report = self
                .block(&evm, &comet, &tree, height)
                .await
                .unwrap_or_else(
                    |e| json!({"height":height, "status":"input_gap", "detail":format!("{e:#}")}),
                );
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
        let ready = counts.len() == 1 && counts.contains_key("verified");
        publish(
            &summary,
            &json!({"from":self.from_block,"to":self.to_block,"counts":counts,"backfill_ready":ready}),
        )?;
        ensure!(ready, "requested range contains unverified blocks");
        Ok(())
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
        let Some(report) = fs::read(path)
            .ok()
            .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        else {
            return Ok(None);
        };
        let name = format!("{height}-{hash}-{version}.json");
        if report["status"] != "verified"
            || report["hash"] != json!(hash)
            || report["parent"] != json!(parent)
            || report["block_file"] != name
            || report["rule"] != version
        {
            return Ok(None);
        }
        let Some(info) =
            tree.get_block_by_id_arc(BlockId::Number(BlockNumberOrTag::Number(height)))?
        else {
            return Ok(None);
        };
        if info.header.hash != hash || info.header.parent_hash != parent {
            return Ok(None);
        }
        let Ok(bytes) = fs::read(self.output_dir.join(name)) else {
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
        let block: Block = evm
            .request(
                "eth_getBlockByNumber",
                rpc_params![format!("{height:#x}"), true],
            )
            .await?;
        ensure!(block.header.number == height, "EVM block height mismatch");
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
        ensure!(
            comet_hash(&tm["block_id"]["hash"])? == hash
                && comet_hash(&tm["block"]["header"]["last_block_id"]["hash"])? == parent,
            "Comet block hash mismatch"
        );
        let max_gas = params["consensus_params"]["block"]["max_gas"]
            .as_str()
            .context("missing max_gas")?
            .parse::<i64>()?;
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
        let mapping = messages::positions(
            tx_array(tm.pointer("/block/data/txs"))?,
            tx_array(results.get("txs_results"))?,
            &hashes,
        )?;
        let mut receipts = Vec::new();
        for (index, tx) in hashes.iter().enumerate() {
            let receipt: alloy::rpc::types::TransactionReceipt = evm
                .request("eth_getTransactionReceipt", rpc_params![tx])
                .await?;
            ensure!(
                receipt.block_hash == Some(hash)
                    && receipt.block_number == Some(height)
                    && receipt.transaction_index == Some(index as u64)
                    && receipt.transaction_hash == *tx,
                "receipt identity mismatch at {index}"
            );
            receipts.push(receipt);
        }
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
        let info = tree
            .get_block_by_id_arc(BlockId::Number(BlockNumberOrTag::Number(height)))?
            .context("archive block missing")?;
        ensure!(
            info.header.hash == hash && info.header.parent_hash == parent,
            "archive block identity mismatch"
        );
        let state = tree
            .state_at(BlockId::Hash(parent.into()))?
            .context("archive parent state missing")?;
        let file = if hashes.is_empty() {
            leafage_evm_rpc::native_block_file(&block.header)
        } else {
            report["receipts"] = serde_json::to_value(&receipts)?;
            if version == "uncovered" {
                report["status"] = json!("rules_uncovered");
                return Ok(report);
            }
            let hooks = [
                "__CronosSendToAccount(address,uint256)",
                "__CronosSendToEthereum(address,uint256,uint256)",
                "__CronosSendToIbc(address,string,uint256)",
                "__CronosSendCroToIbc(address,string,uint256)",
            ]
            .map(keccak256);
            if let Some(index) = receipts.iter().position(|receipt| {
                receipt.logs().iter().any(|log| {
                    log.topics()
                        .first()
                        .is_some_and(|topic| hooks.contains(topic))
                })
            }) {
                report["status"] = json!("native_state_gap");
                report["detail"] = json!(format!(
                    "native post-execution hook at EVM transaction {index}"
                ));
                return Ok(report);
            }
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
            match leafage_evm_rpc::replay_cronos_ethermint(
                leafage_evm_storage::EvmStorageWrapper::new(state),
                &block,
                &receipts,
                max_gas.max(0) as u64,
                &message_gas,
                &messages::begin_transfers(&results)?,
            ) {
                Ok(file) => file,
                Err(err) => {
                    report["status"] = json!("execution_mismatch");
                    report["evm_block"] = serde_json::to_value(&block)?;
                    report["comet_block"] = tm;
                    report["comet_results"] = results;
                    report["consensus_params"] = params;
                    report["detail"] = json!(format!("{err:#}"));
                    return Ok(report);
                }
            }
        };
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
