use super::{crypto, precompile::Failure, L1Fork};
use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use revm::primitives::Bytes;
use sha2::Digest;

pub(super) fn word(value: u64) -> Bytes {
    let mut out = vec![0; 32];
    out[24..].copy_from_slice(&value.to_be_bytes());
    out.into()
}

pub(super) fn required_gas(address: u8, input: &[u8]) -> u64 {
    match address {
        0xfd => 9_000,
        0xfc => {
            let mut gas = 50;
            if input.len() < 192 {
                return gas;
            }
            let a = BigUint::from_bytes_be(&input[128..160]).to_u64();
            let b = BigUint::from_bytes_be(&input[160..192]).to_u64();
            if let (Some(a), Some(b)) = (a, b) {
                let mut n = a.max(b);
                if n > 100_000 {
                    return gas;
                }
                while n > 10 {
                    gas *= 3;
                    n /= 2;
                }
            }
            gas
        }
        0xfb | 0xf4 => 350_000,
        0xfa | 0xf9 | 0xe1 => 1_000,
        0xf8 | 0xf7 | 0xf6 => 10,
        0xf5 => 100,
        0xf3 => 1560 + (input.len() as u64).div_ceil(64) * 12,
        0xf2 | 0xe9 => 600,
        0xf1 | 0xe8 => 12_000,
        0xf0 | 0xe7 => crypto::multiexp_gas(input.len(), 160, 12_000),
        0xef => 800,
        0xee => 45_000,
        0xed => crypto::multiexp_gas(input.len(), 288, 45_000),
        0xec => 65_000 + (input.len() as u64 / 384) * 43_000,
        0xeb => 5_500,
        0xea => 75_000,
        0xe6 => 4_500,
        0xe5 => 55_000,
        0xe4 => crypto::multiexp_gas(input.len(), 288, 55_000),
        0xe3 => 65_000 + (input.len() as u64 / 384) * 55_000,
        0xe2 => match input.split_first() {
            Some((0..=2, data)) => 30 + (data.len() as u64).div_ceil(32) * 6,
            Some((3, data)) => 60 + (data.len() as u64).div_ceil(32) * 12,
            Some((0x10, data)) if data.len() >= 32 => {
                60 + ((data.len() - 32) as u64).div_ceil(32) * 12
            }
            _ => 200,
        },
        _ => 0,
    }
}

pub(super) fn run(address: u8, input: &[u8], fork: L1Fork) -> Result<Bytes, Failure> {
    match address {
        0xfc => fraction(input, fork),
        0xfb => crypto::proof_of_possession(input),
        0xf8 => Ok(word(super::EPOCH_SIZE)),
        0xf7 => Ok(super::consensus::CeloHeader::decode(input)?.number_word()),
        0xf6 => Ok(super::consensus::CeloHeader::decode(input)?
            .hash()
            .as_slice()
            .to_vec()
            .into()),
        0xf3 => {
            // Celo imports hdevalence/ed25519consensus: ZIP-215, including
            // small-order points and noncanonical encodings, is intentional.
            let valid = input.len() >= 96
                && ed25519_consensus::VerificationKey::try_from(&input[..32])
                    .and_then(|key| {
                        key.verify(
                            &ed25519_consensus::Signature::try_from(&input[32..96])?,
                            &input[96..],
                        )
                    })
                    .is_ok();
            Ok(word(u64::from(!valid)))
        }
        0xec..=0xf2 => crypto::bls381::run(0xf2 - address, input),
        0xeb => revm::precompile::bls12_381::map_fp_to_g1::map_fp_to_g1(input, u64::MAX)
            .map(|r| r.bytes)
            .map_err(|_| Failure::Invalid),
        0xea => revm::precompile::bls12_381::map_fp2_to_g2::map_fp2_to_g2(input, u64::MAX)
            .map(|r| r.bytes)
            .map_err(|_| Failure::Invalid),
        0xe3..=0xe9 => crypto::bls377::run(0xe9 - address, input),
        0xe2 => cip20(input),
        _ => Err(Failure::Invalid),
    }
}

fn pad(bytes: Vec<u8>) -> Vec<u8> {
    let mut result = vec![0; 32usize.saturating_sub(bytes.len())];
    result.extend(bytes);
    result
}

fn fraction(input: &[u8], fork: L1Fork) -> Result<Bytes, Failure> {
    if input.len() < 192 || (fork >= L1Fork::Gingerbread && input.len() != 192) {
        return Err(Failure::Invalid);
    }
    let words: Vec<_> = input[..192]
        .chunks_exact(32)
        .map(BigUint::from_bytes_be)
        .collect();
    let exponent = words[4]
        .to_u32()
        .filter(|n| *n <= 100_000)
        .ok_or(Failure::Invalid)?;
    let decimals = words[5]
        .to_u32()
        .filter(|n| *n <= 100_000)
        .ok_or(Failure::Invalid)?;
    let numerator = &words[0] * words[2].pow(exponent);
    let denominator = &words[1] * words[3].pow(exponent);
    // The old Go pointer comparison fails to catch zero and panics in Div.
    // Fail the EVM call instead of panicking the RPC worker.
    if denominator.is_zero() {
        return Err(Failure::Invalid);
    }
    let scale = BigUint::from(10u8).pow(decimals);
    Ok([
        pad((numerator * &scale / denominator).to_bytes_be()),
        pad(scale.to_bytes_be()),
    ]
    .concat()
    .into())
}

fn cip20(input: &[u8]) -> Result<Bytes, Failure> {
    let (selector, data) = input.split_first().ok_or(Failure::Invalid)?;
    Ok(match selector {
        0 => sha3::Sha3_256::digest(data).to_vec().into(),
        1 => sha3::Sha3_512::digest(data).to_vec().into(),
        2 => sha3::Keccak512::digest(data).to_vec().into(),
        3 => sha2::Sha512::digest(data).to_vec().into(),
        0x10 => {
            if data.len() < 32 {
                return Err(Failure::Invalid);
            }
            let size = if data[0] == 0 {
                32
            } else {
                usize::from(data[0])
            };
            let key_size = usize::from(data[1]);
            if size > 32 || key_size > 32 || data[15] > 32 || data.len() < 32 + key_size {
                return Err(Failure::Invalid);
            }
            let mut offset = [0u8; 8];
            offset[..6].copy_from_slice(&data[8..14]);
            blake2s_simd::Params::new()
                .hash_length(size)
                .key(&data[32..32 + key_size])
                .fanout(data[2])
                .max_depth(data[3])
                .max_leaf_length(u32::from_le_bytes(data[4..8].try_into().unwrap()))
                .node_offset(u64::from_le_bytes(offset))
                .node_depth(data[14])
                .inner_hash_length(usize::from(data[15]))
                .salt(&data[16..24])
                .personal(&data[24..32])
                .hash(&data[32 + key_size..])
                .as_bytes()
                .to_vec()
                .into()
        }
        _ => return Err(Failure::Invalid),
    })
}
