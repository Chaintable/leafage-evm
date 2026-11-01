use crate::pipeline::source::ReplayData;
use alloy::{
    consensus::{proofs::ordered_trie_root_encoded, Eip658Value, Receipt, ReceiptWithBloom},
    primitives::{keccak256, Address, Bloom, Bytes, Signature, B256, U256, U64},
    rpc::types::{Header, Log},
};
use alloy_rlp::Encodable;
use anyhow::{bail, ensure, Context, Result};
use leafage_evm_chains::celo::l1::consensus::CeloHeader;
use revm::context_interface::transaction::AccessList;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Transaction {
    pub hash: B256,
    pub from: Address,
    pub to: Option<Address>,
    pub gas: U64,
    pub gas_price: U256,
    pub nonce: U64,
    pub input: Bytes,
    pub value: U256,
    #[serde(rename = "type")]
    pub kind: U64,
    #[serde(default)]
    pub chain_id: Option<U256>,
    #[serde(default)]
    pub max_fee_per_gas: Option<U256>,
    #[serde(default)]
    pub max_priority_fee_per_gas: Option<U256>,
    #[serde(default)]
    pub access_list: AccessList,
    #[serde(default)]
    pub fee_currency: Option<Address>,
    #[serde(default)]
    pub gateway_fee_recipient: Option<Address>,
    #[serde(default)]
    pub gateway_fee: U256,
    #[serde(default)]
    pub eth_compatible: bool,
    pub v: U256,
    pub r: U256,
    pub s: U256,
    pub block_hash: B256,
    pub block_number: U64,
    pub transaction_index: U64,
}

fn encoded<T: Encodable>(value: T) -> Vec<u8> {
    alloy_rlp::encode(value)
}
fn address(value: Option<Address>) -> Vec<u8> {
    encoded(value.as_ref().map(|a| a.as_slice()).unwrap_or(&[]))
}
fn list(fields: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    alloy_rlp::Header {
        list: true,
        payload_length: fields.iter().map(Vec::len).sum(),
    }
    .encode(&mut bytes);
    for field in fields {
        bytes.extend_from_slice(field);
    }
    bytes
}

impl Transaction {
    /// Re-encode and recover the sender, binding every execution field to the
    /// transactions root. Never trust the upstream's `from` or tx hash alone.
    fn verified_encoding(&self) -> Result<Vec<u8>> {
        let ty = self.kind.to::<u64>();
        let mut fields = Vec::new();
        match ty {
            0 => {
                ensure!(
                    self.access_list.is_empty(),
                    "legacy transaction contains an access list"
                );
                ensure!(
                    self.max_fee_per_gas.is_none() && self.max_priority_fee_per_gas.is_none(),
                    "legacy transaction contains dynamic fee fields"
                );
                fields.extend([
                    encoded(self.nonce),
                    encoded(self.gas_price),
                    encoded(self.gas),
                ]);
                if !self.eth_compatible {
                    fields.extend([
                        address(self.fee_currency),
                        address(self.gateway_fee_recipient),
                        encoded(self.gateway_fee),
                    ]);
                } else {
                    ensure!(
                        self.fee_currency.is_none()
                            && self.gateway_fee_recipient.is_none()
                            && self.gateway_fee.is_zero(),
                        "Ethereum legacy transaction contains Celo fee fields"
                    );
                }
                fields.extend([
                    address(self.to),
                    encoded(self.value),
                    encoded(self.input.as_ref()),
                ]);
            }
            1 | 2 | 0x7b | 0x7c => {
                let chain_id = self
                    .chain_id
                    .context("typed transaction is missing chainId")?;
                ensure!(chain_id == U256::from(42220), "wrong transaction chainId");
                fields.extend([encoded(chain_id), encoded(self.nonce)]);
                if ty == 1 {
                    ensure!(
                        self.max_fee_per_gas.is_none() && self.max_priority_fee_per_gas.is_none(),
                        "access-list transaction contains dynamic fee fields"
                    );
                    fields.push(encoded(self.gas_price));
                } else {
                    fields.extend([
                        encoded(
                            self.max_priority_fee_per_gas
                                .context("missing maxPriorityFeePerGas")?,
                        ),
                        encoded(self.max_fee_per_gas.context("missing maxFeePerGas")?),
                    ]);
                }
                fields.push(encoded(self.gas));
                if ty == 0x7c {
                    fields.extend([
                        address(self.fee_currency),
                        address(self.gateway_fee_recipient),
                        encoded(self.gateway_fee),
                    ]);
                } else {
                    ensure!(
                        self.gateway_fee_recipient.is_none() && self.gateway_fee.is_zero(),
                        "unexpected gateway fee"
                    );
                }
                fields.extend([
                    address(self.to),
                    encoded(self.value),
                    encoded(self.input.as_ref()),
                    encoded(&self.access_list),
                ]);
                if ty == 0x7b {
                    fields.push(address(self.fee_currency));
                }
                if ty <= 2 {
                    ensure!(
                        self.fee_currency.is_none(),
                        "Ethereum typed transaction contains feeCurrency"
                    );
                }
            }
            _ => bail!("unsupported Celo L1 transaction type {ty:#x}"),
        }
        let mut signed = fields.clone();
        let parity = if ty == 0 {
            if self.v == U256::from(27) || self.v == U256::from(28) {
                self.v == U256::from(28)
            } else {
                ensure!(self.v >= U256::from(35), "invalid legacy signature v");
                let v = self.v - U256::from(35);
                let chain = v / U256::from(2);
                ensure!(
                    chain == U256::from(42220),
                    "wrong legacy transaction chainId"
                );
                signed.extend([encoded(chain), encoded(0u8), encoded(0u8)]);
                v.bit(0)
            }
        } else {
            ensure!(self.v <= U256::from(1), "invalid typed signature v");
            self.v == U256::from(1)
        };
        let with_type = |payload: Vec<u8>| {
            if ty == 0 {
                payload
            } else {
                let mut out = vec![ty as u8];
                out.extend(payload);
                out
            }
        };
        let signature = Signature::new(self.r, self.s, parity);
        let recovered =
            signature.recover_address_from_prehash(&keccak256(with_type(list(&signed))))?;
        ensure!(
            recovered == self.from,
            "transaction sender mismatch for {}",
            self.hash
        );
        fields.extend([encoded(self.v), encoded(self.r), encoded(self.s)]);
        let bytes = with_type(list(&fields));
        ensure!(
            keccak256(&bytes) == self.hash,
            "transaction hash mismatch for {}",
            self.hash
        );
        Ok(bytes)
    }

    pub fn price(&self, minimum: U256) -> Result<u128> {
        let price = if matches!(self.kind.to::<u64>(), 2 | 0x7b | 0x7c) {
            let cap = self.max_fee_per_gas.context("missing gas fee cap")?;
            let tip = self
                .max_priority_fee_per_gas
                .context("missing gas tip cap")?;
            ensure!(cap >= minimum && cap >= tip, "invalid dynamic fee caps");
            cap.min(minimum.checked_add(tip).context("gas price overflow")?)
        } else {
            self.gas_price
        };
        ensure!(price >= minimum, "gas price below Celo minimum");
        Ok(price.try_into().context("gas price exceeds u128")?)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TransactionReceipt {
    pub transaction_hash: B256,
    pub block_hash: B256,
    pub block_number: U64,
    pub transaction_index: U64,
    pub gas_used: U64,
    pub cumulative_gas_used: U64,
    pub status: U64,
    pub logs: Vec<Log>,
    pub logs_bloom: Bloom,
    #[serde(rename = "type")]
    pub kind: U64,
    pub contract_address: Option<Address>,
}
impl TransactionReceipt {
    fn encoding(&self) -> Result<Vec<u8>> {
        ensure!(self.status.to::<u64>() <= 1, "invalid receipt status");
        let receipt = ReceiptWithBloom::from(Receipt {
            status: Eip658Value::Eip658(self.status == U64::from(1)),
            cumulative_gas_used: self.cumulative_gas_used.to(),
            logs: self.logs.iter().map(|l| l.inner.clone()).collect(),
        });
        ensure!(
            receipt.logs_bloom == self.logs_bloom,
            "receipt bloom mismatch"
        );
        let mut out = if self.kind.is_zero() {
            vec![]
        } else {
            vec![self.kind.try_into().context("invalid receipt type")?]
        };
        receipt.encode(&mut out);
        Ok(out)
    }
}

#[derive(Deserialize)]
pub(super) struct Randomness {
    pub revealed: B256,
    pub committed: B256,
}
pub(super) struct BlockInput {
    pub header: Header,
    pub transactions: Vec<Transaction>,
    pub receipts: Vec<TransactionReceipt>,
    pub randomness: Randomness,
}
impl BlockInput {
    pub fn verified(data: ReplayData, expected: &Header) -> Result<Self> {
        ensure!(
            data.version == 1,
            "unsupported replay input version {}",
            data.version
        );
        let mut raw = data.block;
        // Legacy Celo omits Ethereum-only fields; their protocol values are 0.
        let object = raw.as_object_mut().context("block must be an object")?;
        for (name, value) in [
            ("sha3Uncles", serde_json::to_value(B256::ZERO)?),
            ("mixHash", serde_json::to_value(B256::ZERO)?),
            ("nonce", serde_json::json!("0x0000000000000000")),
        ] {
            object.entry(name).or_insert(value);
        }
        let mut header: Header = serde_json::from_value(raw.clone())?;
        ensure!(
            header.hash == expected.hash
                && CeloHeader::from_header(&header).hash() == expected.hash,
            "replay input is not the canonical archived block"
        );
        let transactions: Vec<Transaction> = serde_json::from_value(raw["transactions"].clone())?;
        let receipts: Vec<TransactionReceipt> = data
            .receipts
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<_, _>>()?;
        ensure!(
            receipts.len() == transactions.len() || receipts.len() == transactions.len() + 1,
            "receipt count mismatch"
        );
        let mut encodings = Vec::new();
        for (i, tx) in transactions.iter().enumerate() {
            ensure!(
                tx.block_hash == header.hash
                    && tx.block_number.to::<u64>() == header.number
                    && tx.transaction_index.to::<usize>() == i,
                "transaction block/index mismatch"
            );
            encodings.push(tx.verified_encoding()?);
        }
        ensure!(
            ordered_trie_root_encoded(&encodings) == header.transactions_root,
            "transaction root mismatch"
        );
        let mut encodings = Vec::new();
        let mut log_index = 0;
        for (i, receipt) in receipts.iter().enumerate() {
            let tx_hash = transactions.get(i).map(|tx| tx.hash).unwrap_or(header.hash);
            ensure!(
                receipt.block_hash == header.hash
                    && receipt.block_number.to::<u64>() == header.number
                    && receipt.transaction_index.to::<usize>() == i
                    && receipt.transaction_hash == tx_hash,
                "receipt block/index/hash mismatch"
            );
            if let Some(tx) = transactions.get(i) {
                ensure!(receipt.kind == tx.kind, "receipt type mismatch");
                let created = tx
                    .to
                    .is_none()
                    .then(|| tx.from.create(tx.nonce.to::<u64>()));
                ensure!(
                    receipt.contract_address == created,
                    "receipt contract address mismatch"
                );
            }
            for log in &receipt.logs {
                ensure!(
                    log.log_index == Some(log_index)
                        && log.block_hash == Some(header.hash)
                        && log.transaction_hash == Some(tx_hash)
                        && log.block_number == Some(header.number)
                        && log.transaction_index == Some(i as u64)
                        && !log.removed,
                    "receipt log identity mismatch"
                );
                log_index += 1;
            }
            encodings.push(receipt.encoding()?);
        }
        ensure!(
            ordered_trie_root_encoded(&encodings) == header.receipts_root,
            "receipts root mismatch"
        );
        let randomness = serde_json::from_value(raw["randomness"].clone())
            .context("missing Celo block randomness")?;
        if header.number < leafage_evm_chains::celo::l1::GINGERBREAD_BLOCK {
            // RPC compatibility values are not part of the legacy consensus
            // header. The blockfile obtains them from canonical system state.
            header.base_fee_per_gas = None;
            header.gas_limit = 0;
            header.difficulty = U256::ZERO;
            header.mix_hash = B256::ZERO;
            header.nonce = Default::default();
        }
        Ok(Self {
            header,
            transactions,
            receipts,
            randomness,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixtures() -> Vec<ReplayData> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/celo_l1");
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "gz"))
            .map(|p| {
                serde_json::from_reader(flate2::read::GzDecoder::new(
                    std::fs::File::open(p).unwrap(),
                ))
                .unwrap()
            })
            .collect()
    }
    fn expected(data: &ReplayData) -> Header {
        Header {
            hash: serde_json::from_value(data.block["hash"].clone()).unwrap(),
            ..Default::default()
        }
    }
    #[test]
    fn celo_native_transaction_and_receipt_roots() {
        let fixtures = fixtures();
        assert_eq!(fixtures.len(), 17);
        for data in fixtures {
            let header = expected(&data);
            let number = data.block["number"].clone();
            let input =
                BlockInput::verified(data, &header).unwrap_or_else(|e| panic!("{number}: {e:#}"));
            if !input.transactions.is_empty() {
                CeloHeader::from_header(&input.header).proposer().unwrap();
            }
        }
    }
    #[test]
    fn celo_replay_rejects_modified_transaction_and_receipt_inputs() {
        let data = fixtures()
            .into_iter()
            .find(|v| v.block["number"] == "0x1d9e273")
            .unwrap();
        let header = expected(&data);
        let mut raw = serde_json::to_value(&data).unwrap();
        raw["block"]["transactions"][0]["gas"] = serde_json::json!("0x100000");
        assert!(BlockInput::verified(serde_json::from_value(raw).unwrap(), &header).is_err());
        let mut raw = serde_json::to_value(&data).unwrap();
        raw["receipts"][0]["status"] = serde_json::json!("0x0");
        assert!(BlockInput::verified(serde_json::from_value(raw).unwrap(), &header).is_err());
        let mut raw = serde_json::to_value(&data).unwrap();
        raw["receipts"][0]["transactionHash"] = serde_json::json!(B256::ZERO);
        assert!(BlockInput::verified(serde_json::from_value(raw).unwrap(), &header).is_err());
        let mut raw = serde_json::to_value(&data).unwrap();
        raw["receipts"][0]["contractAddress"] = serde_json::json!(Address::repeat_byte(0x11));
        assert!(BlockInput::verified(serde_json::from_value(raw).unwrap(), &header).is_err());
        let mut raw = serde_json::to_value(&data).unwrap();
        raw["version"] = serde_json::json!(2);
        assert!(BlockInput::verified(serde_json::from_value(raw).unwrap(), &header).is_err());
    }
}
