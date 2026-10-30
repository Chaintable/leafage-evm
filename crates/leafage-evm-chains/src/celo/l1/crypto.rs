//! Celo's legacy BLS12-377/381 arithmetic and deployed BLS signature encoding.
//! The Blake2Xs try-and-increment code follows celo-org/celo-bls-snark-rs
//! eca15ffed3984601cb441600ba3002d12f339e5f (MIT/Apache-2.0), including its
//! historical sign-bit extraction. It is not the IETF BLS12-381 signature scheme.

use super::precompile::Failure;
use ark_ec_03::{AffineCurve, PairingEngine, ProjectiveCurve};
use ark_ff_03::{BigInteger, Field, One, PrimeField, Zero};
use ark_serialize_03::{CanonicalDeserialize, CanonicalSerialize, Flags};
use num_bigint::{BigInt, BigUint, Sign};
use revm::primitives::Bytes;

pub(super) const DISCOUNT: [u64; 128] = [
    1200, 888, 764, 641, 594, 547, 500, 453, 438, 423, 408, 394, 379, 364, 349, 334, 330, 326, 322,
    318, 314, 310, 306, 302, 298, 294, 289, 285, 281, 277, 273, 269, 268, 266, 265, 263, 262, 260,
    259, 257, 256, 254, 253, 251, 250, 248, 247, 245, 244, 242, 241, 239, 238, 236, 235, 233, 232,
    231, 229, 228, 226, 225, 223, 222, 221, 220, 219, 219, 218, 217, 216, 216, 215, 214, 213, 213,
    212, 211, 211, 210, 209, 208, 208, 207, 206, 205, 205, 204, 203, 202, 202, 201, 200, 199, 199,
    198, 197, 196, 196, 195, 194, 193, 193, 192, 191, 191, 190, 189, 188, 188, 187, 186, 185, 185,
    184, 183, 182, 182, 181, 180, 179, 179, 178, 177, 176, 176, 175, 174,
];

pub(super) fn multiexp_gas(input_len: usize, chunk: usize, cost: u64) -> u64 {
    let k = input_len / chunk;
    if k == 0 {
        return 0;
    }
    (k as u64)
        .saturating_mul(cost)
        .saturating_mul(DISCOUNT[k.min(128) - 1])
        / 1000
}

fn scalar(input: &[u8]) -> ark_ff_03::BigInteger256 {
    ark_ff_03::BigInteger256(std::array::from_fn(|i| {
        u64::from_be_bytes(input[24 - 8 * i..32 - 8 * i].try_into().unwrap())
    }))
}

fn msm_scalar(input: &[u8], count: usize, bits: usize) -> ark_ff_03::BigInteger256 {
    let width = if count < 32 {
        3
    } else {
        (count as f64).ln().ceil() as usize
    };
    let retained = (bits / width + 1) * width;
    let mut value = scalar(input);
    // Celo's old Pippenger implementation sizes its windows from the scalar
    // field, not the input. BLS12-377 can therefore discard the highest bit.
    if retained < 256 {
        value.0[3] &= u64::MAX >> (256 - retained);
    }
    value
}

fn glv_scalars(input: &[u8], q1: &str, q2: u8, b1: &str) -> (BigInt, BigInt) {
    let m = BigUint::from_bytes_be(input);
    let q1 = BigUint::parse_bytes(q1.as_bytes(), 16).unwrap();
    let b1 = BigUint::parse_bytes(b1.as_bytes(), 16).unwrap();
    let b2 = &b1 + BigUint::from(1u8);
    let order = &b1 * &b2 + BigUint::from(1u8);
    let half = BigUint::from(1u8) << 255usize;
    let alpha1 = (&m * q1 + &half) >> 256usize;
    let alpha2 = (&m * BigUint::from(q2) + half) >> 256usize;
    let z1 = (&alpha1 * b1) % &order;
    let z2 = (&alpha2 * b2) % order;
    (
        BigInt::from(m) - BigInt::from(z1) - BigInt::from(alpha2),
        BigInt::from(alpha1) - BigInt::from(z2),
    )
}

fn signed_mul<P: AffineCurve>(point: P, scalar: &BigInt) -> P::Projective {
    let (sign, limbs) = scalar.to_u64_digits();
    let mut repr = <P::ScalarField as PrimeField>::BigInt::default();
    repr.as_mut()[..limbs.len()].copy_from_slice(&limbs);
    let result = point.mul(repr);
    if sign == Sign::Minus {
        -result
    } else {
        result
    }
}

macro_rules! curve {
    ($module:ident, $curve:ident, $engine:ident, $bits:literal, $q1:literal, $q2:literal, $b1:literal, $phi1:expr, $phi2:expr) => {
        pub(super) mod $module {
            use super::*;
            use $curve::{$engine, Fq, Fq2, G1Affine, G1Projective, G2Affine, G2Projective};

            fn field(input: &[u8]) -> Result<Fq, Failure> {
                let f = Fq::from_be_bytes_mod_order(input);
                if encode_field(f).as_slice() != input {
                    return Err(Failure::Invalid);
                }
                Ok(f)
            }
            fn encode_field(f: Fq) -> Vec<u8> {
                let mut out = vec![0; 16];
                out.extend(f.into_repr().to_bytes_be());
                out
            }
            fn g1(input: &[u8]) -> Result<G1Affine, Failure> {
                if input[..128].iter().all(|x| *x == 0) {
                    return Ok(G1Affine::zero());
                }
                let p = G1Affine::new(field(&input[..64])?, field(&input[64..128])?, false);
                if !p.is_on_curve() {
                    return Err(Failure::Invalid);
                }
                Ok(p)
            }
            fn g2(input: &[u8]) -> Result<G2Affine, Failure> {
                if input[..256].iter().all(|x| *x == 0) {
                    return Ok(G2Affine::zero());
                }
                let p = G2Affine::new(
                    Fq2::new(field(&input[..64])?, field(&input[64..128])?),
                    Fq2::new(field(&input[128..192])?, field(&input[192..256])?),
                    false,
                );
                if !p.is_on_curve() {
                    return Err(Failure::Invalid);
                }
                Ok(p)
            }
            fn encode_g1(p: G1Affine) -> Bytes {
                if p.is_zero() {
                    return vec![0; 128].into();
                }
                [encode_field(p.x), encode_field(p.y)].concat().into()
            }
            fn encode_g2(p: G2Affine) -> Bytes {
                if p.is_zero() {
                    return vec![0; 256].into();
                }
                [
                    encode_field(p.x.c0),
                    encode_field(p.x.c1),
                    encode_field(p.y.c0),
                    encode_field(p.y.c1),
                ]
                .concat()
                .into()
            }
            fn mul_g1(p: G1Affine, input: &[u8]) -> G1Affine {
                let (k1, k2) = glv_scalars(input, $q1, $q2, $b1);
                let mut endo = p;
                endo.x *= Fq::new(ark_ff_03::BigInteger384($phi1));
                (signed_mul(p, &k1) + signed_mul(endo, &k2)).into_affine()
            }
            fn mul_g2(p: G2Affine, input: &[u8]) -> G2Affine {
                let (k1, k2) = glv_scalars(input, $q1, $q2, $b1);
                let mut endo = p;
                let phi = Fq::new(ark_ff_03::BigInteger384($phi2));
                endo.x.c0 *= phi;
                endo.x.c1 *= phi;
                (signed_mul(p, &k1) + signed_mul(endo, &k2)).into_affine()
            }
            pub fn run(op: u8, input: &[u8]) -> Result<Bytes, Failure> {
                match op {
                    0 if input.len() == 256 => Ok(encode_g1(
                        (g1(&input[..128])?.into_projective()
                            + g1(&input[128..])?.into_projective())
                        .into_affine(),
                    )),
                    1 if input.len() == 160 => Ok(encode_g1(mul_g1(g1(input)?, &input[128..]))),
                    2 if !input.is_empty() && input.len() % 160 == 0 => {
                        let mut total = G1Projective::zero();
                        for pair in input.chunks_exact(160) {
                            total +=
                                g1(pair)?.mul(msm_scalar(&pair[128..], input.len() / 160, $bits));
                        }
                        Ok(encode_g1(total.into_affine()))
                    }
                    3 if input.len() == 512 => Ok(encode_g2(
                        (g2(&input[..256])?.into_projective()
                            + g2(&input[256..])?.into_projective())
                        .into_affine(),
                    )),
                    4 if input.len() == 288 => Ok(encode_g2(mul_g2(g2(input)?, &input[256..]))),
                    5 if !input.is_empty() && input.len() % 288 == 0 => {
                        let mut total = G2Projective::zero();
                        for pair in input.chunks_exact(288) {
                            total +=
                                g2(pair)?.mul(msm_scalar(&pair[256..], input.len() / 288, $bits));
                        }
                        Ok(encode_g2(total.into_affine()))
                    }
                    6 if !input.is_empty() && input.len() % 384 == 0 => {
                        let mut pairs = Vec::with_capacity(input.len() / 384);
                        for pair in input.chunks_exact(384) {
                            let a = g1(&pair[..128])?;
                            let b = g2(&pair[128..])?;
                            if !a.is_in_correct_subgroup_assuming_on_curve()
                                || !b.is_in_correct_subgroup_assuming_on_curve()
                            {
                                return Err(Failure::Invalid);
                            }
                            pairs.push((a.into(), b.into()));
                        }
                        Ok(super::super::pure::word(u64::from(
                            $engine::product_of_pairings(&pairs).is_one(),
                        )))
                    }
                    _ => Err(Failure::Invalid),
                }
            }
        }
    };
}
// Endomorphisms are the Montgomery field constants from Celo's deployed
// crypto/bls123{77,81}/glv.go. The GLV path is observable for non-subgroup inputs.
curve!(
    bls377,
    ark_bls12_377,
    Bls12_377,
    253,
    "3b3f7aa969fd371607f72ed32af90182c",
    14,
    "452217cc900000010a11800000000000",
    [
        0xdacd106da5847973,
        0xd8fe2454bac2a79a,
        0x1ada4fd6fd832edc,
        0xfb9868449d150908,
        0xd63eb8aeea32285e,
        0x167d6a36f873fd0
    ],
    [
        0x2c766f925a7b8727,
        0x03d7f6b0253d58b5,
        0x838ec0deec122131,
        0xbd5eb3e9f658bb10,
        0x6942bd126ed3e52e,
        0x01673786dd04ed6a
    ]
);
curve!(
    bls381,
    ark_bls12_381,
    Bls12_381,
    255,
    "017c6becf1e01faadd63f6e522f6cfee30",
    2,
    "ac45a4010001a40200000000ffffffff",
    [
        0xcd03c9e48671f071,
        0x5dab22461fcda5d2,
        0x587042afd3851b95,
        0x8eb60ebe01bacb9e,
        0x03f97d6e83d050d2,
        0x18f0206554638741
    ],
    [
        0x30f1361b798a64e8,
        0xf3b8ddab7ece5a2a,
        0x16a8ca3ac61577f7,
        0xc26a2ff874fd029b,
        0x3636b76660701c6e,
        0x051ba4ab241b6160
    ]
);

#[derive(Clone, Copy, Default)]
struct CeloFlags {
    positive: bool,
    infinity: bool,
}
impl Flags for CeloFlags {
    const BIT_SIZE: usize = 2;
    fn u8_bitmask(&self) -> u8 {
        (u8::from(self.positive) << 7) | (u8::from(self.infinity) << 6)
    }
    fn from_u8(value: u8) -> Option<Self> {
        Some(Self {
            positive: value & 128 != 0,
            infinity: value & 64 != 0,
        })
    }
}

fn hash_to_g1(domain: &[u8], message: &[u8]) -> Result<ark_bls12_377::G1Affine, Failure> {
    use ark_bls12_377::{Fq, G1Affine};
    // 48-byte compressed G1 points use a 64-byte Blake2Xs expansion.
    for counter in 0..255u8 {
        let root = blake2s_simd::Params::new()
            .hash_length(32)
            .node_offset(64u64 << 32)
            .personal(domain)
            .to_state()
            .update(&[counter])
            .update(message)
            .finalize();
        let mut candidate = Vec::with_capacity(64);
        for i in 0..2u64 {
            candidate.extend_from_slice(
                blake2s_simd::Params::new()
                    .hash_length(32)
                    .max_leaf_length(32)
                    .inner_hash_length(32)
                    .fanout(0)
                    .max_depth(0)
                    .personal(domain)
                    .node_offset((64u64 << 32) | i)
                    .hash(root.as_bytes())
                    .as_bytes(),
            );
        }
        let positive = candidate[47] & 2 != 0;
        candidate[47] = (candidate[47] & !128) | (u8::from(positive) << 7);
        if let Some((x, flags)) = Fq::from_random_bytes_with_flags::<CeloFlags>(&candidate[..48]) {
            let point = if x.is_zero() && flags.infinity {
                Some(G1Affine::zero())
            } else {
                G1Affine::get_point_from_x(x, flags.positive)
            };
            if let Some(point) = point {
                let point = point.scale_by_cofactor().into_affine();
                if !point.is_zero() {
                    return Ok(point);
                }
            }
        }
    }
    Err(Failure::Invalid)
}

fn public_key(bytes: &[u8]) -> Result<ark_bls12_377::G2Affine, Failure> {
    if bytes.len() != 96 {
        return Err(Failure::Invalid);
    }
    ark_bls12_377::G2Affine::deserialize(bytes).map_err(|_| Failure::Invalid)
}

pub(super) fn public_key_uncompressed(bytes: &[u8]) -> Result<Bytes, Failure> {
    let key = public_key(bytes)?;
    let mut encoded = Vec::new();
    key.serialize_uncompressed(&mut encoded)
        .map_err(|_| Failure::Invalid)?;
    if encoded.len() != 192 {
        return Err(Failure::Invalid);
    }
    let mut output = vec![0u8; 256];
    for (i, limb) in encoded.chunks_exact(48).enumerate() {
        for (j, byte) in limb.iter().enumerate() {
            output[64 * i + 63 - j] = *byte;
        }
    }
    Ok(output.into())
}

pub(super) fn verify_bls(
    domain: &[u8],
    message: &[u8],
    keys: &[&[u8]],
    signature: &[u8],
) -> Result<(), Failure> {
    use ark_bls12_377::{Bls12_377, G1Affine, G2Affine, G2Projective};
    if signature.len() != 48 {
        return Err(Failure::Invalid);
    }
    let signature = G1Affine::deserialize(signature).map_err(|_| Failure::Invalid)?;
    let mut aggregate = G2Projective::zero();
    for key in keys {
        aggregate += public_key(key)?.into_projective();
    }
    let pairs = [
        (
            signature.into(),
            (-G2Affine::prime_subgroup_generator()).into(),
        ),
        (
            hash_to_g1(domain, message)?.into(),
            aggregate.into_affine().into(),
        ),
    ];
    if !Bls12_377::product_of_pairings(&pairs).is_one() {
        return Err(Failure::Invalid);
    }
    Ok(())
}

pub(super) fn proof_of_possession(input: &[u8]) -> Result<Bytes, Failure> {
    if input.len() != 164 {
        return Err(Failure::Invalid);
    }
    verify_bls(b"ULforpop", &input[..20], &[&input[20..116]], &input[116..])?;
    Ok(super::pure::word(1))
}
