//! Native Classic `ecops.cpp` validates BN254 curve points without a G2
//! subgroup check. Nitro and the Ethereum precompiles retain their own checks.
use alloy::primitives::U256;
use ark_bn254::{Bn254, Fq, Fq2, G1Affine, G2Affine};
use ark_ec::{AffineRepr, pairing::Pairing};
use ark_ff::{BigInt, One, PrimeField, Zero};
use revm::precompile::{
    EthPrecompileOutput, EthPrecompileResult, PrecompileHalt,
    bn254::{PAIR_ELEMENT_LEN, pair},
    utilities::bool_to_bytes32,
};

fn fq(bytes: &[u8]) -> Result<Fq, PrecompileHalt> {
    Fq::from_bigint(BigInt::new(U256::from_be_slice(bytes).into_limbs()))
        .ok_or(PrecompileHalt::Bn254FieldPointNotAMember)
}

fn g1(bytes: &[u8]) -> Result<G1Affine, PrecompileHalt> {
    let x = fq(&bytes[..32])?;
    let y = fq(&bytes[32..64])?;
    if x.is_zero() && y.is_zero() {
        return Ok(G1Affine::identity());
    }
    let point = G1Affine::new_unchecked(x, y);
    point
        .is_on_curve()
        .then_some(point)
        .ok_or(PrecompileHalt::Bn254AffineGFailedToCreate)
}

fn g2(bytes: &[u8]) -> Result<G2Affine, PrecompileHalt> {
    // EVM encodes the imaginary coefficient before the real coefficient.
    let x = Fq2::new(fq(&bytes[32..64])?, fq(&bytes[..32])?);
    let y = Fq2::new(fq(&bytes[96..128])?, fq(&bytes[64..96])?);
    if x.is_zero() && y.is_zero() {
        return Ok(G2Affine::identity());
    }
    let point = G2Affine::new_unchecked(x, y);
    point
        .is_on_curve()
        .then_some(point)
        .ok_or(PrecompileHalt::Bn254AffineGFailedToCreate)
}

pub(super) fn run(input: &[u8], gas_limit: u64) -> EthPrecompileResult {
    let count = input.len() / PAIR_ELEMENT_LEN;
    if count > 30 {
        return Err(PrecompileHalt::Bn254PairLength);
    }
    let gas_used = pair::ISTANBUL_PAIR_BASE + count as u64 * pair::ISTANBUL_PAIR_PER_POINT;
    if gas_used > gas_limit {
        return Err(PrecompileHalt::OutOfGas);
    }
    let mut first = Vec::with_capacity(count);
    let mut second = Vec::with_capacity(count);
    // Classic ignores a trailing partial pair. Validate both points before
    // skipping infinity, so malformed G2 points still fail beside zero G1.
    for bytes in input.chunks_exact(PAIR_ELEMENT_LEN) {
        let p = g1(&bytes[..64])?;
        let q = g2(&bytes[64..])?;
        if !p.is_zero() && !q.is_zero() {
            first.push(p);
            second.push(q);
        }
    }
    let success = first.is_empty() || Bn254::multi_pairing(first, second).0.is_one();
    Ok(EthPrecompileOutput::new(gas_used, bool_to_bytes32(success)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(point: G2Affine) -> Vec<u8> {
        use ark_ff::BigInteger;
        [point.x.c1, point.x.c0, point.y.c1, point.y.c0]
            .into_iter()
            .flat_map(|v| v.into_bigint().to_bytes_be())
            .collect()
    }

    #[test]
    fn accepts_on_curve_non_subgroup_points_without_weakening_ethereum() {
        let x = Fq2::new(Fq::from(2), Fq::from(1));
        let point = G2Affine::get_point_from_x_unchecked(x, false).unwrap();
        assert!(point.is_on_curve());
        assert!(!point.is_in_correct_subgroup_assuming_on_curve());
        let input = [vec![0; 64], encode(point)].concat();
        assert_eq!(run(&input, 1_000_000).unwrap().bytes, bool_to_bytes32(true));
        assert!(
            revm::precompile::bn254::run_pair(
                &input,
                pair::ISTANBUL_PAIR_PER_POINT,
                pair::ISTANBUL_PAIR_BASE,
                1_000_000
            )
            .is_err()
        );
    }

    #[test]
    fn preserves_subgroup_pairings_and_validation_with_infinity() {
        let mut input = [vec![0; 64], encode(G2Affine::generator())].concat();
        input[31] = 1;
        input[63] = 2;
        assert_eq!(
            run(&input, 1_000_000).unwrap().bytes,
            bool_to_bytes32(false)
        );
        let negative = [vec![0; 64], encode(-G2Affine::generator())].concat();
        let mut cancel = negative;
        cancel[..64].copy_from_slice(&input[..64]);
        input.extend(cancel);
        assert_eq!(run(&input, 1_000_000).unwrap().bytes, bool_to_bytes32(true));
        assert!(matches!(run(&input, 1), Err(PrecompileHalt::OutOfGas)));
        // G2 infinity cannot hide malformed G1 coordinates.
        let mut invalid = vec![0; 192];
        invalid[31] = 1;
        invalid[63] = 1;
        assert!(run(&invalid, 1_000_000).is_err());
    }
}
