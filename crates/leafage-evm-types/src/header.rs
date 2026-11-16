//! Stable header: Alloy 2.5's field layout and RLP codec with a u128 base fee.
//! Keep the optional-field order in sync with Alloy when upgrading dependencies.
use alloy::primitives::{Address, BlockNumber, Bloom, Bytes, B256, B64, U256};
use alloy_rlp::{length_of_length, BufMut, Decodable, Encodable};
use serde::{Deserialize, Serialize};

pub type Header = alloy::rpc::types::Header<RawHeader>;
pub type Block<T = alloy::rpc::types::Transaction> = alloy::rpc::types::Block<T, Header>;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawHeader {
    pub parent_hash: B256,
    #[serde(rename = "sha3Uncles", alias = "ommersHash")]
    pub ommers_hash: B256,
    #[serde(rename = "miner", alias = "beneficiary")]
    pub beneficiary: Address,
    pub state_root: B256,
    pub transactions_root: B256,
    pub receipts_root: B256,
    pub logs_bloom: Bloom,
    pub difficulty: U256,
    #[serde(with = "alloy::serde::quantity")]
    pub number: BlockNumber,
    #[serde(with = "alloy::serde::quantity")]
    pub gas_limit: u64,
    #[serde(with = "alloy::serde::quantity")]
    pub gas_used: u64,
    #[serde(with = "alloy::serde::quantity")]
    pub timestamp: u64,
    pub extra_data: Bytes,
    pub mix_hash: B256,
    pub nonce: B64,
    #[serde(
        default,
        with = "alloy::serde::quantity::opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub base_fee_per_gas: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withdrawals_root: Option<B256>,
    #[serde(
        default,
        with = "alloy::serde::quantity::opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub blob_gas_used: Option<u64>,
    #[serde(
        default,
        with = "alloy::serde::quantity::opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub excess_blob_gas: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_beacon_block_root: Option<B256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_hash: Option<B256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_access_list_hash: Option<B256>,
    #[serde(
        default,
        with = "alloy::serde::quantity::opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub slot_number: Option<u64>,
}

impl Default for RawHeader {
    fn default() -> Self {
        alloy::consensus::Header::default().into()
    }
}

impl From<alloy::consensus::Header> for RawHeader {
    fn from(header: alloy::consensus::Header) -> Self {
        Self {
            parent_hash: header.parent_hash,
            ommers_hash: header.ommers_hash,
            beneficiary: header.beneficiary,
            state_root: header.state_root,
            transactions_root: header.transactions_root,
            receipts_root: header.receipts_root,
            logs_bloom: header.logs_bloom,
            difficulty: header.difficulty,
            number: header.number,
            gas_limit: header.gas_limit,
            gas_used: header.gas_used,
            timestamp: header.timestamp,
            extra_data: header.extra_data,
            mix_hash: header.mix_hash,
            nonce: header.nonce,
            base_fee_per_gas: header.base_fee_per_gas.map(u128::from),
            withdrawals_root: header.withdrawals_root,
            blob_gas_used: header.blob_gas_used,
            excess_blob_gas: header.excess_blob_gas,
            parent_beacon_block_root: header.parent_beacon_block_root,
            requests_hash: header.requests_hash,
            block_access_list_hash: header.block_access_list_hash,
            slot_number: header.slot_number,
        }
    }
}

impl RawHeader {
    fn header_payload_length(&self) -> usize {
        let mut length = 0;
        length += self.parent_hash.length();
        length += self.ommers_hash.length();
        length += self.beneficiary.length();
        length += self.state_root.length();
        length += self.transactions_root.length();
        length += self.receipts_root.length();
        length += self.logs_bloom.length();
        length += self.difficulty.length();
        length += U256::from(self.number).length();
        length += U256::from(self.gas_limit).length();
        length += U256::from(self.gas_used).length();
        length += self.timestamp.length();
        length += self.extra_data.length();
        length += self.mix_hash.length();
        length += self.nonce.length();

        if let Some(base_fee) = self.base_fee_per_gas {
            length += U256::from(base_fee).length();
        }

        if let Some(root) = self.withdrawals_root {
            length += root.length();
        }

        if let Some(blob_gas_used) = self.blob_gas_used {
            length += U256::from(blob_gas_used).length();
        }

        if let Some(excess_blob_gas) = self.excess_blob_gas {
            length += U256::from(excess_blob_gas).length();
        }

        if let Some(parent_beacon_block_root) = self.parent_beacon_block_root {
            length += parent_beacon_block_root.length();
        }

        if let Some(requests_hash) = self.requests_hash {
            length += requests_hash.length();
        }

        if let Some(block_access_list_hash) = self.block_access_list_hash {
            length += block_access_list_hash.length();
        }

        if let Some(slot_number) = self.slot_number {
            length += U256::from(slot_number).length();
        }

        length
    }
}

impl Encodable for RawHeader {
    fn encode(&self, out: &mut dyn BufMut) {
        let list_header = alloy_rlp::Header {
            list: true,
            payload_length: self.header_payload_length(),
        };
        list_header.encode(out);
        self.parent_hash.encode(out);
        self.ommers_hash.encode(out);
        self.beneficiary.encode(out);
        self.state_root.encode(out);
        self.transactions_root.encode(out);
        self.receipts_root.encode(out);
        self.logs_bloom.encode(out);
        self.difficulty.encode(out);
        U256::from(self.number).encode(out);
        U256::from(self.gas_limit).encode(out);
        U256::from(self.gas_used).encode(out);
        self.timestamp.encode(out);
        self.extra_data.encode(out);
        self.mix_hash.encode(out);
        self.nonce.encode(out);

        // Encode all the fork specific fields
        if let Some(ref base_fee) = self.base_fee_per_gas {
            U256::from(*base_fee).encode(out);
        }

        if let Some(ref root) = self.withdrawals_root {
            root.encode(out);
        }

        if let Some(ref blob_gas_used) = self.blob_gas_used {
            U256::from(*blob_gas_used).encode(out);
        }

        if let Some(ref excess_blob_gas) = self.excess_blob_gas {
            U256::from(*excess_blob_gas).encode(out);
        }

        if let Some(ref parent_beacon_block_root) = self.parent_beacon_block_root {
            parent_beacon_block_root.encode(out);
        }

        if let Some(ref requests_hash) = self.requests_hash {
            requests_hash.encode(out);
        }

        if let Some(ref block_access_list_hash) = self.block_access_list_hash {
            block_access_list_hash.encode(out);
        }

        if let Some(ref slot_number) = self.slot_number {
            U256::from(*slot_number).encode(out);
        }
    }

    fn length(&self) -> usize {
        let mut length = 0;
        length += self.header_payload_length();
        length += length_of_length(length);
        length
    }
}

impl Decodable for RawHeader {
    fn decode(input: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let mut buf = *input;
        let rlp_head = alloy_rlp::Header::decode(&mut buf)?;
        if !rlp_head.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        if buf.len() < rlp_head.payload_length {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let (payload, rest) = buf.split_at(rlp_head.payload_length);
        let mut buf = payload;
        let started_len = buf.len();
        let mut this = Self {
            parent_hash: Decodable::decode(&mut buf)?,
            ommers_hash: Decodable::decode(&mut buf)?,
            beneficiary: Decodable::decode(&mut buf)?,
            state_root: Decodable::decode(&mut buf)?,
            transactions_root: Decodable::decode(&mut buf)?,
            receipts_root: Decodable::decode(&mut buf)?,
            logs_bloom: Decodable::decode(&mut buf)?,
            difficulty: Decodable::decode(&mut buf)?,
            number: u64::decode(&mut buf)?,
            gas_limit: u64::decode(&mut buf)?,
            gas_used: u64::decode(&mut buf)?,
            timestamp: Decodable::decode(&mut buf)?,
            extra_data: Decodable::decode(&mut buf)?,
            mix_hash: Decodable::decode(&mut buf)?,
            nonce: B64::decode(&mut buf)?,
            base_fee_per_gas: None,
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_hash: None,
            block_access_list_hash: None,
            slot_number: None,
        };
        if started_len - buf.len() < rlp_head.payload_length {
            this.base_fee_per_gas = Some(u128::decode(&mut buf)?);
        }

        // Withdrawals root for post-shanghai headers
        if started_len - buf.len() < rlp_head.payload_length {
            this.withdrawals_root = Some(Decodable::decode(&mut buf)?);
        }

        // Blob gas used and excess blob gas for post-cancun headers
        if started_len - buf.len() < rlp_head.payload_length {
            this.blob_gas_used = Some(u64::decode(&mut buf)?);
        }

        if started_len - buf.len() < rlp_head.payload_length {
            this.excess_blob_gas = Some(u64::decode(&mut buf)?);
        }

        // Decode parent beacon block root.
        if started_len - buf.len() < rlp_head.payload_length {
            this.parent_beacon_block_root = Some(B256::decode(&mut buf)?);
        }

        // Decode requests hash.
        if started_len - buf.len() < rlp_head.payload_length {
            this.requests_hash = Some(B256::decode(&mut buf)?);
        }

        // Decode block access list hash.
        if started_len - buf.len() < rlp_head.payload_length {
            this.block_access_list_hash = Some(B256::decode(&mut buf)?);
        }

        // Decode slot number.
        if started_len - buf.len() < rlp_head.payload_length {
            this.slot_number = Some(u64::decode(&mut buf)?);
        }

        let consumed = started_len - buf.len();
        if consumed != rlp_head.payload_length {
            return Err(alloy_rlp::Error::ListLengthMismatch {
                expected: rlp_head.payload_length,
                got: consumed,
            });
        }
        *input = rest;
        Ok(this)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_header_round_trip() {
        for fee in [
            None,
            Some(0),
            Some(u64::MAX as u128),
            Some(1_000_000_000_000_000_000_001),
            Some(u128::MAX),
        ] {
            let raw = RawHeader {
                base_fee_per_gas: fee,
                ..Default::default()
            };
            let block = crate::DebankBlock {
                base_fee_per_gas: fee.unwrap_or_default(),
                ..Default::default()
            };
            let json = serde_json::to_value(&block).unwrap();
            assert_eq!(
                json["base_fee_per_gas"].to_string(),
                block.base_fee_per_gas.to_string()
            );
            let header = crate::HeaderInfo::new(Header {
                inner: raw.clone(),
                ..Default::default()
            });
            let json = serde_json::to_string(&header).unwrap();
            assert_eq!(
                serde_json::from_str::<crate::HeaderInfo>(&json).unwrap(),
                header
            );
            let mut bytes = alloy_rlp::encode(&raw);
            bytes.extend_from_slice(b"{}");
            let mut input = bytes.as_slice();
            assert_eq!(RawHeader::decode(&mut input).unwrap(), raw);
            assert_eq!(input, b"{}");
            if fee.is_none_or(|fee| fee <= u64::MAX as u128) {
                let original = alloy::consensus::Header {
                    base_fee_per_gas: fee.map(|v| v as u64),
                    ..Default::default()
                };
                assert_eq!(alloy_rlp::encode(&raw), alloy_rlp::encode(&original));
            }
        }
    }
}
