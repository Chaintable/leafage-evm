//! Minimal ABI codec with the exact semantics of
//! `execution/ethereum/core/contract/abi_decode.hpp` / `abi_encode.hpp`.
//!
//! Decoding is *not* strict ABI: a fixed value is read from the low bytes of
//! its 32 byte word and the high bytes are ignored (`abi_decode_fixed` copies
//! `sizeof(T)` bytes from offset `32 - sizeof(T)`), so a non canonical word is
//! accepted where a strict decoder would revert. This must stay byte for byte
//! compatible with the node.

use super::error::StakingError;
use revm::primitives::{Address, U256};

pub(crate) struct Decoder<'a> {
    data: &'a [u8],
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// `abi_decode_fixed`: next 32 byte word.
    fn word(&mut self) -> Result<&'a [u8], StakingError> {
        if self.data.len() < 32 {
            return Err(StakingError::InputTooShort);
        }
        let (word, rest) = self.data.split_at(32);
        self.data = rest;
        Ok(word)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, StakingError> {
        Ok(self.word()?[31])
    }

    pub(crate) fn u32(&mut self) -> Result<u32, StakingError> {
        let word = self.word()?;
        Ok(u32::from_be_bytes(word[28..32].try_into().unwrap()))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, StakingError> {
        let word = self.word()?;
        Ok(u64::from_be_bytes(word[24..32].try_into().unwrap()))
    }

    pub(crate) fn u256(&mut self) -> Result<U256, StakingError> {
        Ok(U256::from_be_slice(self.word()?))
    }

    pub(crate) fn address(&mut self) -> Result<Address, StakingError> {
        Ok(Address::from_slice(&self.word()?[12..32]))
    }

    /// `abi_decode_bytes_tail<N>`: a dynamic `bytes` value in the tail whose
    /// length must be exactly `N`.
    pub(crate) fn bytes_tail<const N: usize>(&mut self) -> Result<[u8; N], StakingError> {
        let length = self.u256()?;
        if length != U256::from(N) {
            return Err(StakingError::LengthMismatch);
        }
        let padded = N.div_ceil(32) * 32;
        if self.data.len() < padded {
            return Err(StakingError::InputTooShort);
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.data[..N]);
        self.data = &self.data[padded..];
        Ok(out)
    }
}

pub(crate) fn encode_u256(value: U256) -> [u8; 32] {
    value.to_be_bytes()
}

pub(crate) fn encode_u64(value: u64) -> [u8; 32] {
    encode_u256(U256::from(value))
}

pub(crate) fn encode_bool(value: bool) -> [u8; 32] {
    encode_u64(u64::from(value))
}

pub(crate) fn encode_address(address: Address) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(address.as_slice());
    out
}

/// `AbiEncoder`: static values in the head, dynamic values in the tail with
/// their offsets resolved in `finish`.
#[derive(Default)]
pub(crate) struct Encoder {
    head: Vec<u8>,
    tail: Vec<u8>,
    unresolved: Vec<(usize, usize)>,
}

impl Encoder {
    fn add_static(&mut self, word: [u8; 32]) {
        self.head.extend_from_slice(&word);
    }

    fn add_dynamic(&mut self, data: Vec<u8>) {
        self.unresolved.push((self.head.len(), self.tail.len()));
        self.head.extend_from_slice(&[0u8; 32]);
        self.tail.extend_from_slice(&data);
    }

    pub(crate) fn add_u256(&mut self, value: U256) -> &mut Self {
        self.add_static(encode_u256(value));
        self
    }

    pub(crate) fn add_u64(&mut self, value: u64) -> &mut Self {
        self.add_static(encode_u64(value));
        self
    }

    pub(crate) fn add_bool(&mut self, value: bool) -> &mut Self {
        self.add_static(encode_bool(value));
        self
    }

    pub(crate) fn add_address(&mut self, address: Address) -> &mut Self {
        self.add_static(encode_address(address));
        self
    }

    /// `abi_encode_bytes`.
    pub(crate) fn add_bytes(&mut self, data: &[u8]) -> &mut Self {
        let mut out = Vec::with_capacity(32 + data.len().div_ceil(32) * 32);
        out.extend_from_slice(&encode_u64(data.len() as u64));
        out.extend_from_slice(data);
        out.resize(32 + data.len().div_ceil(32) * 32, 0);
        self.add_dynamic(out);
        self
    }

    /// `abi_encode_uint_array` for `u64_be` elements.
    pub(crate) fn add_u64_array(&mut self, values: &[u64]) -> &mut Self {
        let mut out = Vec::with_capacity(32 * (values.len() + 1));
        out.extend_from_slice(&encode_u64(values.len() as u64));
        for value in values {
            out.extend_from_slice(&encode_u64(*value));
        }
        self.add_dynamic(out);
        self
    }

    /// `abi_encode_address_array`.
    pub(crate) fn add_address_array(&mut self, values: &[Address]) -> &mut Self {
        let mut out = Vec::with_capacity(32 * (values.len() + 1));
        out.extend_from_slice(&encode_u64(values.len() as u64));
        for value in values {
            out.extend_from_slice(&encode_address(*value));
        }
        self.add_dynamic(out);
        self
    }

    /// `AbiEncoder::encode_final`.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        let head_len = self.head.len();
        for (head_offset, tail_offset) in &self.unresolved {
            let offset = encode_u64((head_len + tail_offset) as u64);
            self.head[*head_offset..*head_offset + 32].copy_from_slice(&offset);
        }
        self.head.extend_from_slice(&self.tail);
        self.head
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::primitives::address;

    #[test]
    fn decoder_reads_low_bytes_and_ignores_high_bytes() {
        let mut word = [0xffu8; 32];
        word[24..].copy_from_slice(&7u64.to_be_bytes());
        let mut d = Decoder::new(&word);
        assert_eq!(d.u64().unwrap(), 7);
        assert!(d.is_empty());

        let mut d = Decoder::new(&word[..31]);
        assert_eq!(d.u64(), Err(StakingError::InputTooShort));
    }

    #[test]
    fn bytes_tail_requires_exact_length() {
        let mut input = Vec::new();
        input.extend_from_slice(&encode_u64(33));
        input.extend_from_slice(&[0xab; 64]);
        let mut d = Decoder::new(&input);
        let bytes = d.bytes_tail::<33>().unwrap();
        assert_eq!(bytes, [0xab; 33]);
        assert!(d.is_empty());

        let mut d = Decoder::new(&input);
        assert_eq!(d.bytes_tail::<32>(), Err(StakingError::LengthMismatch));

        let mut d = Decoder::new(&input[..40]);
        assert_eq!(d.bytes_tail::<33>(), Err(StakingError::InputTooShort));
    }

    #[test]
    fn encoder_resolves_dynamic_offsets_like_abi_encoder() {
        let addr = address!("00000000000000000000000000000000000000aa");
        let mut e = Encoder::default();
        e.add_bool(true)
            .add_address(addr)
            .add_address_array(&[addr, Address::ZERO]);
        let out = e.finish();
        // head: bool, address, offset(0x60); tail: len 2, addr, zero
        assert_eq!(out.len(), 32 * 6);
        assert_eq!(out[..32], encode_u64(1));
        assert_eq!(out[32..64], encode_address(addr));
        assert_eq!(out[64..96], encode_u64(0x60));
        assert_eq!(out[96..128], encode_u64(2));
        assert_eq!(out[128..160], encode_address(addr));
        assert_eq!(out[160..192], [0u8; 32]);
    }

    #[test]
    fn encoder_pads_bytes_to_word_boundary() {
        let mut e = Encoder::default();
        e.add_u64(1).add_bytes(&[1u8; 33]).add_bytes(&[2u8; 48]);
        let out = e.finish();
        // head 3 words; tail: (len + 64) + (len + 64)
        assert_eq!(out.len(), 96 + 96 + 96);
        assert_eq!(out[32..64], encode_u64(96));
        assert_eq!(out[64..96], encode_u64(96 + 96));
        assert_eq!(out[96..128], encode_u64(33));
        assert_eq!(out[128 + 33], 0);
        assert_eq!(out[192..224], encode_u64(48));
    }
}
