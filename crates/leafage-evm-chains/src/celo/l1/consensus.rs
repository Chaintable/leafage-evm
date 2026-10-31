//! Consensus inputs are reconstructed from canonical archive headers. Missing
//! headers are node errors, never a successful empty validator set or seal.

use super::{crypto, precompile::Failure, EPOCH_SIZE};
use alloy::consensus::BlockHeader;
use alloy_rlp::{Decodable, Encodable};
use alloy_rlp_derive::{RlpDecodable, RlpEncodable};
use revm::primitives::{keccak256, Address, Bytes, B256, U256};
use std::sync::{Arc, OnceLock};

pub trait CeloChainData {
    fn header(&self, number: u64) -> Result<CeloHeader, String>;
}

#[derive(Clone, Debug, Default)]
struct Integer(Bytes);
impl Decodable for Integer {
    fn decode(input: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let b = Bytes::decode(input)?;
        if b.first() == Some(&0) {
            return Err(alloy_rlp::Error::LeadingZero);
        }
        Ok(Self(b))
    }
}
impl Encodable for Integer {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.0.as_ref().encode(out);
    }
    fn length(&self) -> usize {
        self.0.as_ref().length()
    }
}
impl Integer {
    fn bit(&self, index: usize) -> bool {
        self.0
            .len()
            .checked_sub(index / 8 + 1)
            .map(|n| self.0[n] & (1 << (index % 8)) != 0)
            .unwrap_or(false)
    }
    fn padded(&self) -> Bytes {
        let mut out = vec![0; 32usize.saturating_sub(self.0.len())];
        out.extend_from_slice(&self.0);
        out.into()
    }
}

#[derive(Clone, Debug, Default, RlpDecodable, RlpEncodable)]
struct Seal {
    bitmap: Integer,
    signature: Bytes,
    round: Integer,
}

#[derive(Clone, Debug, RlpDecodable, RlpEncodable)]
struct Extra {
    added: Vec<Address>,
    keys: Vec<Bytes>,
    removed: Integer,
    proposer: Bytes,
    seal: Seal,
    parent_seal: Seal,
}

impl Extra {
    fn decode(data: &[u8]) -> Result<Self, Failure> {
        let encoded = data.get(32..).ok_or(Failure::Invalid)?;
        let extra: Self = alloy_rlp::decode_exact(encoded).map_err(|_| Failure::Invalid)?;
        if extra.keys.iter().any(|k| k.len() != 96) {
            return Err(Failure::Invalid);
        }
        Ok(extra)
    }
}

#[derive(Clone, Debug)]
pub struct CeloHeader {
    fields: Vec<Bytes>,
}

fn integer(value: U256) -> Bytes {
    let bytes = value.to_be_bytes::<32>();
    bytes[bytes.iter().position(|b| *b != 0).unwrap_or(32)..]
        .to_vec()
        .into()
}

impl CeloHeader {
    pub fn from_header(header: &impl BlockHeader) -> Self {
        // Legacy archive headers can contain Ethereum RPC compatibility
        // defaults (notably EMPTY_OMMER_ROOT_HASH). The consensus encoding is
        // selected by Celo's fork height, never by those synthetic fields.
        let fields = if header.number() < super::GINGERBREAD_BLOCK {
            vec![
                header.parent_hash().as_slice().to_vec().into(),
                header.beneficiary().as_slice().to_vec().into(),
                header.state_root().as_slice().to_vec().into(),
                header.transactions_root().as_slice().to_vec().into(),
                header.receipts_root().as_slice().to_vec().into(),
                header.logs_bloom().as_slice().to_vec().into(),
                integer(U256::from(header.number())),
                integer(U256::from(header.gas_used())),
                integer(U256::from(header.timestamp())),
                header.extra_data().clone(),
            ]
        } else {
            vec![
                header.parent_hash().as_slice().to_vec().into(),
                header.ommers_hash().as_slice().to_vec().into(),
                header.beneficiary().as_slice().to_vec().into(),
                header.state_root().as_slice().to_vec().into(),
                header.transactions_root().as_slice().to_vec().into(),
                header.receipts_root().as_slice().to_vec().into(),
                header.logs_bloom().as_slice().to_vec().into(),
                integer(header.difficulty()),
                integer(U256::from(header.number())),
                integer(U256::from(header.gas_limit())),
                integer(U256::from(header.gas_used())),
                integer(U256::from(header.timestamp())),
                header.extra_data().clone(),
                header
                    .mix_hash()
                    .unwrap_or_default()
                    .as_slice()
                    .to_vec()
                    .into(),
                header
                    .nonce()
                    .unwrap_or_default()
                    .as_slice()
                    .to_vec()
                    .into(),
                integer(U256::from(header.base_fee_per_gas().unwrap_or_default())),
            ]
        };
        Self { fields }
    }

    pub(crate) fn decode(encoded: &[u8]) -> Result<Self, Failure> {
        let mut input = encoded;
        let h = alloy_rlp::Header::decode(&mut input).map_err(|_| Failure::Invalid)?;
        if !h.list || h.payload_length != input.len() {
            return Err(Failure::Invalid);
        }
        let mut fields = Vec::new();
        while !input.is_empty() {
            fields.push(Bytes::decode(&mut input).map_err(|_| Failure::Invalid)?);
        }
        let legacy = fields.len() == 10;
        if !legacy && fields.len() != 15 && fields.len() != 16 {
            return Err(Failure::Invalid);
        }
        let fixed: &[(usize, usize)] = if legacy {
            &[(0, 32), (1, 20), (2, 32), (3, 32), (4, 32), (5, 256)]
        } else {
            &[
                (0, 32),
                (1, 32),
                (2, 20),
                (3, 32),
                (4, 32),
                (5, 32),
                (6, 256),
                (13, 32),
                (14, 8),
            ]
        };
        if fixed.iter().any(|(i, n)| fields[*i].len() != *n) {
            return Err(Failure::Invalid);
        }
        let numbers: &[usize] = if legacy {
            &[6, 7, 8]
        } else {
            &[7, 8, 9, 10, 11]
        };
        if numbers.iter().any(|i| fields[*i].first() == Some(&0))
            || (!legacy && fields.len() == 16 && fields[15].first() == Some(&0))
        {
            return Err(Failure::Invalid);
        }
        let small: &[usize] = if legacy { &[7, 8] } else { &[9, 10, 11] };
        if small.iter().any(|i| fields[*i].len() > 8) {
            return Err(Failure::Invalid);
        }
        if !legacy {
            if fields.len() == 15 {
                fields.push(Bytes::new());
            }
            if fields[1].iter().all(|b| *b == 0) {
                fields = [0, 2, 3, 4, 5, 6, 8, 10, 11, 12]
                    .iter()
                    .map(|i| fields[*i].clone())
                    .collect();
            }
        }
        Ok(Self { fields })
    }

    fn extra_index(&self) -> usize {
        if self.fields.len() == 10 {
            9
        } else {
            12
        }
    }
    fn extra(&self) -> Result<Extra, Failure> {
        Extra::decode(&self.fields[self.extra_index()])
    }
    fn number_bytes(&self) -> &Bytes {
        &self.fields[if self.fields.len() == 10 { 6 } else { 8 }]
    }
    fn number(&self) -> Result<u64, Failure> {
        let b = self.number_bytes();
        if b.len() > 8 {
            return Err(Failure::Invalid);
        }
        Ok(b.iter().fold(0u64, |v, b| (v << 8) | u64::from(*b)))
    }
    pub(crate) fn number_word(&self) -> Bytes {
        Integer(self.number_bytes().clone()).padded()
    }

    pub fn encode(&self) -> Bytes {
        let payload_len = self.fields.iter().map(|f| f.as_ref().length()).sum();
        let mut out = Vec::new();
        alloy_rlp::Header {
            list: true,
            payload_length: payload_len,
        }
        .encode(&mut out);
        for field in &self.fields {
            field.as_ref().encode(&mut out);
        }
        out.into()
    }

    pub fn hash(&self) -> B256 {
        let mut header = self.clone();
        if let Ok(mut extra) = self.extra() {
            extra.seal = Seal::default();
            let i = self.extra_index();
            let mut bytes = self.fields[i][..32].to_vec();
            extra.encode(&mut bytes);
            header.fields[i] = bytes.into();
        }
        keccak256(header.encode())
    }

    /// Istanbul proposer, which can differ from the fee-recipient coinbase.
    pub fn proposer(&self) -> Result<Address, String> {
        let mut extra = self.extra().map_err(|_| "invalid Istanbul extra data")?;
        let signature =
            alloy::primitives::Signature::from_raw(&extra.proposer).map_err(|e| e.to_string())?;
        extra.proposer = Bytes::new();
        extra.seal = Seal::default();
        let i = self.extra_index();
        let mut bytes = self.fields[i][..32].to_vec();
        extra.encode(&mut bytes);
        let mut header = self.clone();
        header.fields[i] = bytes.into();
        // Native GetSignatureAddress hashes the sigHash once more.
        signature
            .recover_address_from_prehash(&keccak256(keccak256(header.encode())))
            .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug)]
struct Validator {
    address: Address,
    key: Bytes,
}
static VALIDATORS: OnceLock<moka::sync::Cache<B256, Arc<Vec<Validator>>>> = OnceLock::new();

/// CIP-21 uptime for the validators signing the ending epoch. Historical
/// replay uses that epoch's canonical parent-seal bitmaps, never a live
/// consensus monitor (which may already belong to another epoch).
pub fn epoch_uptimes(
    chain: &dyn CeloChainData,
    number: u64,
    lookback: u64,
) -> Result<Vec<(Address, U256)>, String> {
    if number == 0 || number % EPOCH_SIZE != 0 || !(3..=720).contains(&lookback) {
        return Err("invalid Celo uptime window".into());
    }
    let validators = validators(chain, number).map_err(|e| format!("{e:?}"))?;
    let first = number - EPOCH_SIZE + 1;
    let start = first + lookback - 1;
    let end = number - 2;
    let mut last = vec![0; validators.len()];
    let mut up = vec![0u64; validators.len()];
    let mut previous = chain.header(first)?.hash();
    for height in first + 1..number {
        let header = chain.header(height)?;
        if header.fields[0].as_ref() != previous.as_slice() {
            return Err(format!("non-contiguous Celo uptime headers at {height}"));
        }
        let extra = header
            .extra()
            .map_err(|_| format!("invalid uptime header at {height}"))?;
        let signed = height - 1;
        for (i, last) in last.iter_mut().enumerate() {
            if extra.parent_seal.bitmap.bit(i) {
                *last = signed;
            }
            if signed >= start && signed <= end && *last >= signed + 1 - lookback {
                up[i] += 1;
            }
        }
        previous = header.hash();
    }
    let scale = U256::from(10).pow(U256::from(24));
    Ok(validators
        .iter()
        .zip(up)
        .map(|(v, n)| {
            (
                v.address,
                U256::from(n) * scale / U256::from(end - start + 1),
            )
        })
        .collect())
}

#[cfg(test)]
mod uptime_tests {
    use super::*;

    struct Headers(Vec<CeloHeader>);
    impl CeloChainData for Headers {
        fn header(&self, number: u64) -> Result<CeloHeader, String> {
            self.0
                .get(number as usize)
                .cloned()
                .ok_or_else(|| "missing header".into())
        }
    }

    #[test]
    fn epoch_uptime_uses_parent_seals_and_excludes_the_last_two_blocks() {
        let addresses: Vec<_> = (1..=4).map(Address::with_last_byte).collect();
        let mut chain = Headers(Vec::new());
        let mut previous = B256::ZERO;
        for number in 0..=EPOCH_SIZE {
            // Validator 0 signs throughout; validator 1 never signs;
            // validator 2 signs only block 1; validator 3 signs only
            // the blocks whose votes arrive in the final two headers. The header carries its parent's votes.
            let bitmap =
                1 | if number == 2 { 4 } else { 0 } | if number >= EPOCH_SIZE - 1 { 8 } else { 0 };
            let extra = Extra {
                added: if number == 0 {
                    addresses.clone()
                } else {
                    Vec::new()
                },
                keys: if number == 0 {
                    vec![vec![0; 96].into(); 4]
                } else {
                    Vec::new()
                },
                removed: Integer::default(),
                proposer: Bytes::new(),
                seal: Seal::default(),
                parent_seal: Seal {
                    bitmap: Integer(vec![bitmap].into()),
                    ..Default::default()
                },
            };
            let mut bytes = vec![0; 32];
            extra.encode(&mut bytes);
            let mut fields = vec![Bytes::new(); 10];
            fields[0] = previous.as_slice().to_vec().into();
            fields[6] = integer(U256::from(number));
            fields[9] = bytes.into();
            let header = CeloHeader { fields };
            previous = header.hash();
            chain.0.push(header);
        }
        let scores = epoch_uptimes(&chain, EPOCH_SIZE, 3).unwrap();
        let scale = U256::from(10).pow(U256::from(24));
        assert_eq!(scores[0], (addresses[0], scale));
        assert_eq!(scores[1].1, U256::ZERO);
        assert_eq!(scores[2].1, scale / U256::from(EPOCH_SIZE - 4));
        // Parent seal in header n-1 covers n-2, the last monitored block.
        assert_eq!(scores[3].1, scale / U256::from(EPOCH_SIZE - 4));
        chain.0[10].fields[0] = vec![0; 32].into();
        assert!(epoch_uptimes(&chain, EPOCH_SIZE, 3)
            .unwrap_err()
            .contains("non-contiguous"));
    }
}

fn validators(chain: &dyn CeloChainData, requested: u64) -> Result<Arc<Vec<Validator>>, Failure> {
    if requested == 0 {
        return Ok(Arc::new(Vec::new()));
    }
    let cache = VALIDATORS.get_or_init(|| moka::sync::Cache::new(4096));
    let mut boundary = (requested - 1) / EPOCH_SIZE * EPOCH_SIZE;
    let mut pending = Vec::new();
    let mut current = loop {
        let header = chain.header(boundary).map_err(Failure::Node)?;
        let hash = header.hash();
        if let Some(cached) = cache.get(&hash) {
            break cached.as_ref().clone();
        }
        let extra = header
            .extra()
            .map_err(|_| Failure::Node(format!("invalid Celo consensus header at {boundary}")))?;
        pending.push((hash, extra));
        if boundary == 0 {
            break Vec::new();
        }
        boundary -= EPOCH_SIZE;
    };
    for (hash, extra) in pending.into_iter().rev() {
        if extra.added.len() != extra.keys.len()
            || (current.len()..extra.removed.0.len() * 8).any(|i| extra.removed.bit(i))
        {
            return Err(Failure::Node("invalid Celo validator delta".into()));
        }
        current = current
            .into_iter()
            .enumerate()
            .filter_map(|(i, v)| (!extra.removed.bit(i)).then_some(v))
            .collect();
        for (address, key) in extra.added.into_iter().zip(extra.keys) {
            if current.iter().any(|v| v.address == address) {
                return Err(Failure::Node("duplicate Celo validator".into()));
            }
            current.push(Validator { address, key });
        }
        cache.insert(hash, Arc::new(current.clone()));
    }
    Ok(Arc::new(current))
}

pub(super) fn run(
    address: u8,
    input: &[u8],
    current: u64,
    gingerbread: bool,
    chain: &dyn CeloChainData,
) -> Result<Bytes, Failure> {
    if address == 0xf4 {
        let header = CeloHeader::decode(input)?;
        let number = header.number()?;
        if number == 0 || number > current {
            return Err(Failure::Invalid);
        }
        let extra = header.extra()?;
        let validators = validators(chain, number)?;
        let keys: Vec<_> = validators
            .iter()
            .enumerate()
            .filter(|(i, _)| extra.seal.bitmap.bit(*i))
            .map(|(_, v)| v.key.as_ref())
            .collect();
        if keys.len() < (validators.len() * 2).div_ceil(3) {
            return Err(Failure::Invalid);
        }
        let mut message = header.hash().as_slice().to_vec();
        message.extend_from_slice(&extra.seal.round.0);
        message.push(2); // Istanbul MsgCommit
        crypto::verify_bls(b"ULforxof", &message, &keys, &extra.seal.signature)?;
        return Ok(extra.seal.bitmap.padded());
    }
    let index_request = address == 0xfa || address == 0xe1;
    let size = if index_request { 64 } else { 32 };
    if input.len() < size || (address == 0xfa && gingerbread && input.len() != size) {
        return Err(Failure::Invalid);
    }
    let number = U256::from_be_slice(&input[size - 32..size]);
    if number > U256::from(current) {
        return Err(Failure::Invalid);
    }
    let number = number.to::<u64>();
    if address == 0xf5 {
        if number == 0 || current.saturating_sub(number) >= 4 * EPOCH_SIZE {
            return Err(Failure::Invalid);
        }
        let header = chain.header(number).map_err(Failure::Node)?;
        return Ok(header
            .extra()
            .map_err(|_| Failure::Node("invalid Celo parent-seal header".into()))?
            .parent_seal
            .bitmap
            .padded());
    }
    let validators = validators(chain, number)?;
    if address == 0xf9 {
        return Ok(super::pure::word(validators.len() as u64));
    }
    let index = U256::from_be_slice(&input[..32]);
    if index >= U256::from(validators.len()) {
        return Err(Failure::Invalid);
    }
    let validator = &validators[index.to::<usize>()];
    if address == 0xe1 {
        crypto::public_key_uncompressed(&validator.key)
    } else {
        let mut out = vec![0; 12];
        out.extend_from_slice(validator.address.as_slice());
        Ok(out.into())
    }
}
