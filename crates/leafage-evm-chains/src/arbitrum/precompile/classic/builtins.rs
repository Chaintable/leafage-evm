//! Classic Mini builtin dispatch. Keep this separate from Nitro's ABI and
//! storage: even the argument checks and STATICCALL rules differ.
use alloy::primitives::{Bytes, U256};
use revm::interpreter::CallInputs;

pub(super) enum Outcome {
    Revert,
    Return(Bytes),
    Unavailable,
}

// Mini's bytearray_get256 pads missing calldata with zeroes.
pub(super) fn word(data: &[u8], offset: usize) -> U256 {
    let mut bytes = [0; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = data.get(offset + i).copied().unwrap_or(0);
    }
    U256::from_be_bytes(bytes)
}

fn words(values: &[U256]) -> Outcome {
    Outcome::Return(values.iter().flat_map(|v| v.to_be_bytes::<32>()).collect())
}

pub(super) fn padded_bytes_length(data: &[u8], start: usize, size_at: usize) -> bool {
    word(data, size_at)
        .checked_add(U256::from(31))
        .map(|n| n / U256::from(32) * U256::from(32))
        .and_then(|n| n.checked_add(U256::from(start)))
        == Some(U256::from(data.len()))
}

#[derive(Clone, Copy)]
enum Length {
    Exact(usize),
    AtLeast(usize),
}

pub(super) fn run(inputs: &CallInputs, data: &[u8], depth: usize) -> Outcome {
    use Length::{AtLeast, Exact};
    let Some(selector) = data.get(..4) else {
        return Outcome::Revert;
    };
    let selector = u32::from_be_bytes(selector.try_into().unwrap());
    let id = inputs.bytecode_address.as_slice()[19];
    // These checks come from arb_os/*.mini, not Solidity mutability annotations.
    // If a method reads private state before rejecting malformed input, either
    // outcome still reverts, so it is safe to perform the rejection first.
    let (length, forbid_static) = match (id, selector) {
        (0x66, 0x4420e486) => (Exact(36), true), // register
        (0x66, 0xd4b6b5da | 0xa5025222 | 0x8a186788 | 0xf6a455a2) => (Exact(36), false),
        (0x66, 0x949d225d) => (Exact(4), false),     // size
        (0x66, 0x31862ada) => (AtLeast(132), false), // decompress
        (0x67, 0x375a7c7f) => (Exact(132), true),    // register
        (0x67, 0x857cdbb8) => (Exact(36), false),    // getPublicKey
        (0x68, 0xce2ae159) => (AtLeast(68), true),   // upload
        (0x68, 0x88987068) => (Exact(36), false),    // size
        (0x68, 0xb464631b) => (Exact(68), false),    // get
        (0x69, 0xfbe6e022) => (AtLeast(165), true),  // installAccount
        (0x69, 0xd56aa31f | 0x7b510fe8 | 0xbb3480f9) => (Exact(36), false),
        (0x69, 0x1d79f325 | 0xe30443bc) => (Exact(68), false), // setNonce / setBalance
        (0x69, 0x978190f1 | 0xafefabf7) => (AtLeast(100), false), // setCode / setState
        (0x69, 0xf0c26aff) => (Exact(100), false),             // store
        (0x6b, 0xc3bf429d | 0x31acdf5e | 0xdebb08f5 | 0xca4ba78c) => (Exact(4), true),
        (
            0x6b,
            0xda47668c | 0x84352b19 | 0xc746c8f4 | 0x471eab5c | 0x4290549e | 0x481f8dbf
            | 0x8792701a,
        ) => (Exact(36), true),
        (0x6b, 0xb3def0d7 | 0x966e2505 | 0xe1e639cf | 0x842fbc79 | 0x929f1b01) => (Exact(68), true),
        (
            0x6b,
            0xc060180d | 0x10c2e0b9 | 0xd12ac0e1 | 0x2816aba5 | 0xa88bae30 | 0x817ef62e
            | 0x516b4e0f | 0xbafcd57f,
        ) => (Exact(4), false),
        (0x6b, 0x973f9730 | 0xbe8c97b0 | 0x26ef7f68) => (Exact(36), false),
        (0x6b, 0xcad462d4 | 0x2b16e6c0) => (Exact(68), false),
        (0x6b, 0x56331f75) => (AtLeast(68), true), // continueCodeUpload
        (0x6b, 0x5b0b7f1e) => (AtLeast(132), true), // deployContract
        (0x6c, 0xba9c916e | 0x7a1ea732) => (Exact(36), false),
        (0x6c, 0x41b247a8 | 0x02199f34 | 0x612af178 | 0x055f362f) => (Exact(4), false),
        (0x6c, 0x4290549e) => (Exact(36), true), // setL1GasPriceEstimate
        (0x6c, 0xc6f7de0e) => (AtLeast(4), false), // getCurrentTxL1GasFees ignores trailing data
        (0x6d, 0x52f10740 | 0x9c2c5bb5 | 0x049764af) => (Exact(36), false),
        (0x6d, 0x6e928a6e | 0x0ffd6650) => (Exact(36), true),
        (0x6d, 0x875883f2) => (Exact(4), false),
        (0x6d, 0x29149799) => (Exact(68), true), // setFeeCollector
        (0x6d, 0x5be6888b) => (Exact(68), false), // setTxBaseFee has no static guard
        (0x6e, 0xeda1122c | 0xf0b21a41 | 0xc4d252f5) => (Exact(36), true),
        (0x6e, 0x81e6e083) => (Exact(4), false),
        (0x6e, 0x9f1025c6 | 0xf88029dc | 0xb16607e5 | 0xba20dda4) => (Exact(36), false),
        (0x6f, 0xc59d4847) => (Exact(4), false), // getStats
        // 0xc8 is revertPrecompile in EVM execution. The native RPC's node
        // interface intercept is a separate path, not an EVM precompile.
        _ => return Outcome::Revert,
    };
    if (forbid_static && inputs.is_static)
        || match length {
            Exact(n) => data.len() != n,
            AtLeast(n) => data.len() < n,
        }
    {
        return Outcome::Revert;
    }
    if id == 0x69 && selector != 0xbb3480f9 && !inputs.caller.is_zero() {
        return Outcome::Revert;
    }
    match (id, selector) {
        (0x66, 0x31862ada) => return decompress(data),
        // The address table always starts with address zero at index zero;
        // these entries cannot be removed. No private lookup is necessary.
        (0x66, 0x4420e486 | 0xd4b6b5da | 0xa5025222 | 0xf6a455a2)
            if data[16..36].iter().all(|b| *b == 0) =>
        {
            return match selector {
                0xa5025222 => words(&[U256::ONE]),
                0xf6a455a2 => words(&[U256::from(32), U256::ONE, U256::from(0x80) << 248]),
                _ => words(&[U256::ZERO]),
            };
        }
        (0x66, 0x8a186788) if word(data, 4).is_zero() => return words(&[U256::ZERO]),
        (0x68, 0xce2ae159) => {
            if word(data, 4) != U256::from(32) || !valid_function_table(data) {
                return Outcome::Revert;
            }
        }
        (0x69, 0xbb3480f9) if word(data, 4).is_zero() => return Outcome::Return(Bytes::new()),
        (0x69, 0x978190f1 | 0xafefabf7) => {
            if word(data, 68) > U256::from(data.len() - 100) {
                return Outcome::Revert;
            }
        }
        (0x6b, 0x5b0b7f1e) if !padded_bytes_length(data, 132, 100) => return Outcome::Revert,
        // Both operations are restricted to the top-level AVM call frame.
        (0x6b, 0xb3def0d7) | (0x6e, 0xeda1122c) if depth != 1 => return Outcome::Revert,
        _ => {}
    }
    // A valid method that needs unexported state must not become a catchable
    // revert (or a fabricated zero). This also covers unknown historical
    // method availability, e.g. getCurrentTxL1GasFees before its introduction.
    Outcome::Unavailable
}

// Model a Mini ByteArray slice without allocating its declared length. Reads
// beyond calldata are zero-padded, but slices outside 2^64 are empty.
struct Stream<'a> {
    data: &'a [u8],
    start: u64,
    length: u64,
    position: u64,
}

impl<'a> Stream<'a> {
    fn new(data: &'a [u8], start: U256, length: U256) -> Option<Self> {
        let limit = U256::from(1u128 << 64);
        if start >= limit || length.is_zero() || length > limit || start + length > limit {
            return None;
        }
        // start is nonzero for both ABI payloads, so length is below 2^64.
        Some(Self {
            data,
            start: start.to(),
            length: length.to(),
            position: 0,
        })
    }

    fn byte(&mut self) -> Option<u8> {
        if self.position >= self.length {
            return None;
        }
        let index = self.start + self.position;
        self.position += 1;
        Some(
            usize::try_from(index)
                .ok()
                .and_then(|i| self.data.get(i))
                .copied()
                .unwrap_or(0),
        )
    }

    fn uint(&mut self) -> Option<(u8, U256)> {
        let prefix = self.byte()?;
        if prefix < 0x80 {
            return Some((prefix, U256::from(prefix)));
        }
        let length = prefix - 0x80;
        if length > 32 {
            return None;
        }
        let mut value = U256::ZERO;
        for _ in 0..length {
            value = (value << 8) | U256::from(self.byte()?);
        }
        Some((prefix, value))
    }
}

fn decompress(data: &[u8]) -> Outcome {
    let offset = word(data, 36);
    let length = word(data, 68);
    if word(data, 4) != U256::from(64) || offset >= length {
        return Outcome::Revert;
    }
    let Some(mut stream) = Stream::new(data, offset.wrapping_add(U256::from(100)), length - offset)
    else {
        return Outcome::Revert;
    };
    let Some((prefix, value)) = stream.uint() else {
        return Outcome::Revert;
    };
    if prefix < 0x94 && !value.is_zero() {
        return Outcome::Unavailable; // nonzero table index
    }
    if value >> 160 != U256::ZERO {
        return Outcome::Revert;
    }
    words(&[value, offset.wrapping_add(U256::from(stream.position))])
}

fn valid_function_table(data: &[u8]) -> bool {
    let Some(mut stream) = Stream::new(data, U256::from(68), word(data, 36)) else {
        return false;
    };
    let Some((_, entries)) = stream.uint() else {
        return false;
    };
    if entries > U256::from(1024) {
        return false;
    }
    for _ in 0..entries.to::<usize>() {
        for _ in 0..5 {
            // four selector bytes and one payable byte
            if stream.byte().is_none() {
                return false;
            }
        }
        if stream.uint().is_none() {
            return false;
        }
    }
    true // Mini ignores trailing bytes and accepts noncanonical RLP integers.
}
