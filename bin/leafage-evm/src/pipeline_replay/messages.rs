//! Read Cosmos message positions from the signed TxRaw. DeliverTx events
//! supply the executed Ethereum hash, including when MsgEthereumTx.hash
//! was omitted by the sender.
use alloy::primitives::B256;
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use prost::Message;
use serde::Serialize;
use serde_json::Value;

#[derive(Message)]
struct TxRaw {
    #[prost(bytes, tag = "1")]
    body_bytes: Vec<u8>,
}

#[derive(Message)]
struct TxBody {
    #[prost(message, repeated, tag = "1")]
    messages: Vec<Any>,
}

#[derive(Message)]
struct Any {
    #[prost(string, tag = "1")]
    type_url: String,
}

#[derive(Serialize)]
pub(super) struct Position {
    pub cosmos_tx: usize,
    pub msg: usize,
    pub type_url: String,
    pub cosmos_code: u64,
    pub evm_tx: Option<usize>,
}

fn event_hashes(events: &[Value]) -> Result<Vec<B256>> {
    let mut hashes = Vec::new();
    for event in events.iter().filter(|e| e["type"] == "ethereum_tx") {
        let attributes = event["attributes"]
            .as_array()
            .context("missing ethereum_tx attributes")?;
        // Huygen started emitting an ante event with the same hash.
        // The execution event also includes the transaction amount.
        if !attributes.iter().any(|attr| {
            attr["key"].as_str().is_some_and(|key| {
                key == "amount" || STANDARD.decode(key).ok().as_deref() == Some(b"amount")
            })
        }) {
            continue;
        }
        for attr in attributes {
            let key = attr["key"].as_str().context("missing attribute key")?;
            let value = attr["value"].as_str().context("missing attribute value")?;
            if key == "ethereumTxHash" {
                hashes.push(value.parse()?);
            } else if STANDARD.decode(key).ok().as_deref() == Some(b"ethereumTxHash") {
                hashes.push(std::str::from_utf8(&STANDARD.decode(value)?)?.parse()?);
            }
        }
    }
    Ok(hashes)
}

pub(super) fn positions(
    raw: &[Value],
    results: &[Value],
    expected: &[B256],
) -> Result<Vec<Position>> {
    ensure!(
        raw.len() == results.len(),
        "Cosmos body/result count differs"
    );
    let mut positions = Vec::new();
    let mut index = 0;
    for (cosmos_tx, (raw, result)) in raw.iter().zip(results).enumerate() {
        let bytes = STANDARD.decode(raw.as_str().context("invalid Cosmos tx")?)?;
        let tx = TxRaw::decode(bytes.as_slice())?;
        let body = TxBody::decode(tx.body_bytes.as_slice())?;
        ensure!(!body.messages.is_empty(), "empty Cosmos tx {cosmos_tx}");
        let code = result["code"]
            .as_u64()
            .context("missing Cosmos result code")?;
        let events = match &result["events"] {
            Value::Null => &[][..],
            Value::Array(list) => list,
            _ => anyhow::bail!("invalid Cosmos events"),
        };
        let hashes = event_hashes(events)?;
        let mut hashes = hashes.iter();
        for (msg, message) in body.messages.into_iter().enumerate() {
            let evm_tx = if code == 0 && message.type_url == "/ethermint.evm.v1.MsgEthereumTx" {
                let hash = hashes
                    .next()
                    .with_context(|| format!("missing EVM result at ({cosmos_tx},{msg})"))?;
                ensure!(
                    expected.get(index) == Some(hash),
                    "EVM hash mismatch at ({cosmos_tx},{msg},{index})"
                );
                index += 1;
                Some(index - 1)
            } else {
                None
            };
            positions.push(Position {
                cosmos_tx,
                msg,
                type_url: message.type_url,
                cosmos_code: code,
                evm_tx,
            });
        }
        ensure!(
            hashes.next().is_none(),
            "extra EVM result in Cosmos tx {cosmos_tx}"
        );
    }
    ensure!(index == expected.len(), "unmatched EVM transactions");
    Ok(positions)
}

/// BeginBlock bank events from the historical SDK (including the events
/// rewrapped by newer Comet RPC servers). Only basecro is an EVM balance.
pub(super) fn begin_transfers(
    results: &Value,
) -> Result<Vec<(alloy::primitives::Address, bool, alloy::primitives::U256)>> {
    use alloy::primitives::{Address, U256};
    let (events, wrapped) = if let Some(events) = results.get("begin_block_events") {
        (events, false)
    } else {
        (
            results
                .get("finalize_block_events")
                .context("block hook events missing")?,
            true,
        )
    };
    let mut transfers = Vec::new();
    if events.is_null() {
        return Ok(transfers);
    }
    for event in events.as_array().context("invalid block hook events")? {
        let receiver = match event["type"].as_str() {
            Some("coin_spent") => false,
            Some("coin_received") => true,
            _ => continue,
        };
        let attrs = event["attributes"]
            .as_array()
            .context("missing bank attributes")?;
        let get = |key: &str| -> Result<String> {
            for attr in attrs {
                let k = attr["key"].as_str().context("missing key")?;
                let v = attr["value"].as_str().context("missing value")?;
                if k == key {
                    return Ok(v.to_owned());
                }
                if STANDARD.decode(k).ok().as_deref() == Some(key.as_bytes()) {
                    return Ok(String::from_utf8(STANDARD.decode(v)?)?);
                }
            }
            anyhow::bail!("missing bank attribute {key}")
        };
        if wrapped && get("mode")? != "BeginBlock" {
            continue;
        }
        for coin in get("amount")?.split(',') {
            let n = coin.bytes().take_while(u8::is_ascii_digit).count();
            if &coin[n..] != "basecro" {
                continue;
            }
            let amount = U256::from_str_radix(&coin[..n], 10)?;
            let (prefix, bytes) =
                bech32::decode(&get(if receiver { "receiver" } else { "spender" })?)?;
            ensure!(
                prefix.as_str() == "crc" && bytes.len() == 20,
                "invalid Cronos bank address"
            );
            transfers.push((Address::from_slice(&bytes), receiver, amount));
        }
    }
    Ok(transfers)
}
