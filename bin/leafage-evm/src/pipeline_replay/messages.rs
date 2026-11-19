//! Read Cosmos message positions from the signed TxRaw. DeliverTx events
//! supply the executed Ethereum hash, including when MsgEthereumTx.hash
//! was omitted by the sender.
use alloy::{
    consensus::Transaction,
    primitives::{Address, B256, U256},
    rpc::types::Block,
};
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
        // Some binaries also emit an ante event with the same hash.
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

/// Comet 0.37 sends base64 attributes. Later versions expose UTF-8 strings.
fn attribute(event: &Value, name: &str) -> Result<Option<String>> {
    let attrs = event["attributes"]
        .as_array()
        .context("missing bank attributes")?;
    for attr in attrs.iter().rev() {
        let key = attr["key"].as_str().context("missing attribute key")?;
        if key == name {
            return Ok(Some(
                attr["value"]
                    .as_str()
                    .context("missing attribute value")?
                    .to_owned(),
            ));
        }
        if STANDARD.decode(key).ok().as_deref() == Some(name.as_bytes()) {
            let value = attr["value"].as_str().context("missing attribute value")?;
            return Ok(Some(String::from_utf8(STANDARD.decode(value)?)?));
        }
    }
    Ok(None)
}

fn bank_transfer(event: &Value) -> Result<Option<(Address, bool, U256)>> {
    let receiver = match event["type"].as_str() {
        Some("coin_spent") => false,
        Some("coin_received") => true,
        _ => return Ok(None),
    };
    // x/bank emits sdk.Coins.String(), with integer amounts in base units.
    // An empty string represents a zero movement. Other denoms have no EVM balance.
    let Some(amount) = attribute(event, "amount")? else {
        return Ok(None);
    };
    let mut amount_basecro = U256::ZERO;
    for coin in amount.split(',') {
        let n = coin.bytes().take_while(u8::is_ascii_digit).count();
        if &coin[n..] == "basecro" {
            amount_basecro = U256::from_str_radix(&coin[..n], 10)?;
        }
    }
    if amount_basecro.is_zero() {
        return Ok(None);
    }
    let key = if receiver { "receiver" } else { "spender" };
    let Some(encoded) = attribute(event, key)? else {
        return Ok(None);
    };
    let Ok(decoded) = bech32::primitives::decode::CheckedHrpstring::new::<bech32::Bech32>(&encoded)
    else {
        return Ok(None);
    };
    let bytes: Vec<_> = decoded.byte_iter().collect();
    // ADR-028 module addresses can be 32 bytes; writer ignores them.
    if decoded.hrp().as_str() != "crc" || bytes.len() != 20 {
        return Ok(None);
    }
    Ok(Some((
        Address::from_slice(&bytes),
        receiver,
        amount_basecro,
    )))
}

fn events(value: &Value) -> Result<&[Value]> {
    match value {
        Value::Null => Ok(&[]),
        Value::Array(events) => Ok(events),
        _ => anyhow::bail!("invalid Cosmos event list"),
    }
}

/// Apply BeginBlock transfers before transaction zero.
pub(super) fn begin_transfers(results: &Value) -> Result<Vec<(Address, bool, U256)>> {
    let (raw, wrapped) = if let Some(events) = results.get("begin_block_events") {
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
    for event in events(raw)? {
        if let Some(transfer) = bank_transfer(event)? {
            if !wrapped || attribute(event, "mode")?.as_deref() == Some("BeginBlock") {
                transfers.push(transfer);
            }
        }
    }
    Ok(transfers)
}

/// Match writer bankdiff.CoinTouchedAddresses and the tx from/to addresses
/// supplied to fillAbsoluteState. Includes failed-tx events and end-block fees.
pub(super) fn extend_storage_contracts(
    contracts: &mut Vec<String>,
    block: &Block,
    results: &Value,
) -> Result<()> {
    for tx in block.transactions.txns() {
        contracts.push(format!("{:#x}", tx.inner.signer()));
        if let Some(to) = tx.to() {
            contracts.push(format!("{to:#x}"));
        }
    }
    let mut scan = |list: &Value| -> Result<()> {
        for event in events(list)? {
            if let Some((address, _, _)) = bank_transfer(event)? {
                contracts.push(format!("{address:#x}"));
            }
        }
        Ok(())
    };
    if let Some(finalize) = results.get("finalize_block_events") {
        scan(finalize)?;
    } else {
        scan(
            results
                .get("begin_block_events")
                .context("begin-block events missing")?,
        )?;
        scan(
            results
                .get("end_block_events")
                .context("end-block events missing")?,
        )?;
    }
    for tx in super::tx_array(results.get("txs_results"))? {
        scan(&tx["events"])?;
    }
    contracts.sort_unstable();
    contracts.dedup();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bank_addresses_include_begin_end_and_failed_transactions() {
        fn coin(addr: u8, amount: &str, mode: &str, legacy_encoding: bool) -> Value {
            let address = bech32::encode::<bech32::Bech32>(
                bech32::Hrp::parse("crc").unwrap(),
                Address::with_last_byte(addr).as_slice(),
            )
            .unwrap();
            let attr = |key: &str, value: &str| {
                if legacy_encoding {
                    json!({"key": STANDARD.encode(key), "value": STANDARD.encode(value)})
                } else {
                    json!({"key": key, "value": value})
                }
            };
            json!({"type":"coin_received", "attributes":[attr("receiver", &address), attr("amount", amount), attr("mode", mode)]})
        }
        let result = json!({
            "finalize_block_events": [coin(1,"0basecro","BeginBlock",false), coin(2,"9basecro,2ibc/ABCD","BeginBlock",false), coin(3,"1basecro","EndBlock",false)],
            "txs_results":[{"code":1, "events":[coin(4,"2basecro","",true), coin(5,"4other","",true)]}]
        });
        let begin = begin_transfers(&result).unwrap();
        assert_eq!(begin, [(Address::with_last_byte(2), true, U256::from(9))]);
        let mut touched = vec![format!("{:#x}", Address::with_last_byte(2))];
        extend_storage_contracts(&mut touched, &Block::default(), &result).unwrap();
        assert_eq!(
            touched,
            [2, 3, 4].map(|last| format!("{:#x}", Address::with_last_byte(last)))
        );
    }
}
