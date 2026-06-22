use revm::precompile::{
    blake2, bls12_381,
    bls12_381_const::{
        DISCOUNT_TABLE_G1_MSM, DISCOUNT_TABLE_G2_MSM, G1_ADD_ADDRESS, G1_MSM_ADDRESS,
        G1_MSM_INPUT_LENGTH, G2_ADD_ADDRESS, G2_MSM_ADDRESS, G2_MSM_INPUT_LENGTH,
        MAP_FP2_TO_G2_ADDRESS, MAP_FP_TO_G1_ADDRESS, PAIRING_ADDRESS, PAIRING_INPUT_LENGTH,
    },
    bls12_381_utils::msm_required_gas,
    bn254, u64_to_address, Precompile, PrecompileError, PrecompileId, PrecompileResult,
};

const BLAKE2F_INPUT_LENGTH: usize = 213;

const GFROUND: u64 = 22;
const BN254_ADD_GAS: u64 = 540;
const BN254_MUL_GAS: u64 = 12_600;
const BN254_PAIR_BASE_GAS: u64 = 67_500;
const BN254_PAIR_PER_POINT_GAS: u64 = 51_000;

const BLS12_G1_ADD_GAS: u64 = 1_050;
const BLS12_G1_MSM_GAS: u64 = 73_200;
const BLS12_G2_ADD_GAS: u64 = 1_620;
const BLS12_G2_MSM_GAS: u64 = 144_000;
const BLS12_PAIRING_BASE_GAS: u64 = 109_330;
const BLS12_PAIRING_PER_PAIR_GAS: u64 = 94_540;
const BLS12_MAP_G1_GAS: u64 = 15_400;
const BLS12_MAP_G2_GAS: u64 = 66_640;

const BN254_ADD: Precompile =
    Precompile::new(PrecompileId::Bn254Add, bn254::add::ADDRESS, bn254_add);
const BN254_MUL: Precompile =
    Precompile::new(PrecompileId::Bn254Mul, bn254::mul::ADDRESS, bn254_mul);
const BN254_PAIR: Precompile =
    Precompile::new(PrecompileId::Bn254Pairing, bn254::pair::ADDRESS, bn254_pair);
const BLAKE2F: Precompile = Precompile::new(PrecompileId::Blake2F, u64_to_address(9), blake2f);
const BLS12_G1_ADD: Precompile =
    Precompile::new(PrecompileId::Bls12G1Add, G1_ADD_ADDRESS, bls12_g1_add);
const BLS12_G1_MSM: Precompile =
    Precompile::new(PrecompileId::Bls12G1Msm, G1_MSM_ADDRESS, bls12_g1_msm);
const BLS12_G2_ADD: Precompile =
    Precompile::new(PrecompileId::Bls12G2Add, G2_ADD_ADDRESS, bls12_g2_add);
const BLS12_G2_MSM: Precompile =
    Precompile::new(PrecompileId::Bls12G2Msm, G2_MSM_ADDRESS, bls12_g2_msm);
const BLS12_PAIRING: Precompile =
    Precompile::new(PrecompileId::Bls12Pairing, PAIRING_ADDRESS, bls12_pairing);
const BLS12_MAP_G1: Precompile = Precompile::new(
    PrecompileId::Bls12MapFpToGp1,
    MAP_FP_TO_G1_ADDRESS,
    bls12_map_g1,
);
const BLS12_MAP_G2: Precompile = Precompile::new(
    PrecompileId::Bls12MapFp2ToGp2,
    MAP_FP2_TO_G2_ADDRESS,
    bls12_map_g2,
);

pub(super) fn precompiles() -> [Precompile; 11] {
    [
        BN254_ADD,
        BN254_MUL,
        BN254_PAIR,
        BLAKE2F,
        BLS12_G1_ADD,
        BLS12_G1_MSM,
        BLS12_G2_ADD,
        BLS12_G2_MSM,
        BLS12_PAIRING,
        BLS12_MAP_G1,
        BLS12_MAP_G2,
    ]
}

fn bn254_add(input: &[u8], gas_limit: u64) -> PrecompileResult {
    bn254::run_add(input, BN254_ADD_GAS, gas_limit)
}

fn bn254_mul(input: &[u8], gas_limit: u64) -> PrecompileResult {
    bn254::run_mul(input, BN254_MUL_GAS, gas_limit)
}

fn bn254_pair(input: &[u8], gas_limit: u64) -> PrecompileResult {
    bn254::run_pair(
        input,
        BN254_PAIR_PER_POINT_GAS,
        BN254_PAIR_BASE_GAS,
        gas_limit,
    )
}

fn blake2f(input: &[u8], gas_limit: u64) -> PrecompileResult {
    if input.len() != BLAKE2F_INPUT_LENGTH {
        return Err(PrecompileError::Blake2WrongLength);
    }

    let rounds = u32::from_be_bytes(input[..4].try_into().unwrap()) as u64;
    let gas = rounds * GFROUND;
    if gas > gas_limit {
        return Err(PrecompileError::OutOfGas);
    }

    let mut output = blake2::run(input, gas_limit)?;
    output.gas_used = gas;
    Ok(output)
}

fn bls12_g1_add(input: &[u8], gas_limit: u64) -> PrecompileResult {
    run_repriced(
        input,
        gas_limit,
        BLS12_G1_ADD_GAS,
        bls12_381::g1_add::g1_add,
    )
}

fn bls12_g1_msm(input: &[u8], gas_limit: u64) -> PrecompileResult {
    let gas = bls_msm_gas(
        input.len(),
        G1_MSM_INPUT_LENGTH,
        BLS12_G1_MSM_GAS,
        &DISCOUNT_TABLE_G1_MSM,
    );
    run_repriced(input, gas_limit, gas, bls12_381::g1_msm::g1_msm)
}

fn bls12_g2_add(input: &[u8], gas_limit: u64) -> PrecompileResult {
    run_repriced(
        input,
        gas_limit,
        BLS12_G2_ADD_GAS,
        bls12_381::g2_add::g2_add,
    )
}

fn bls12_g2_msm(input: &[u8], gas_limit: u64) -> PrecompileResult {
    let gas = bls_msm_gas(
        input.len(),
        G2_MSM_INPUT_LENGTH,
        BLS12_G2_MSM_GAS,
        &DISCOUNT_TABLE_G2_MSM,
    );
    run_repriced(input, gas_limit, gas, bls12_381::g2_msm::g2_msm)
}

fn bls12_pairing(input: &[u8], gas_limit: u64) -> PrecompileResult {
    let gas = (input.len() / PAIRING_INPUT_LENGTH) as u64 * BLS12_PAIRING_PER_PAIR_GAS
        + BLS12_PAIRING_BASE_GAS;
    run_repriced(input, gas_limit, gas, bls12_381::pairing::pairing)
}

fn bls12_map_g1(input: &[u8], gas_limit: u64) -> PrecompileResult {
    run_repriced(
        input,
        gas_limit,
        BLS12_MAP_G1_GAS,
        bls12_381::map_fp_to_g1::map_fp_to_g1,
    )
}

fn bls12_map_g2(input: &[u8], gas_limit: u64) -> PrecompileResult {
    run_repriced(
        input,
        gas_limit,
        BLS12_MAP_G2_GAS,
        bls12_381::map_fp2_to_g2::map_fp2_to_g2,
    )
}

fn run_repriced(
    input: &[u8],
    gas_limit: u64,
    gas: u64,
    run: fn(&[u8], u64) -> PrecompileResult,
) -> PrecompileResult {
    if gas > gas_limit {
        return Err(PrecompileError::OutOfGas);
    }

    let mut output = run(input, gas_limit)?;
    output.gas_used = gas;
    Ok(output)
}

fn bls_msm_gas(
    input_len: usize,
    item_len: usize,
    multiplication_cost: u64,
    discount_table: &[u16],
) -> u64 {
    msm_required_gas(input_len / item_len, discount_table, multiplication_cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn precompile(address: u64) -> Precompile {
        precompiles()
            .into_iter()
            .find(|precompile| *precompile.address() == u64_to_address(address))
            .unwrap()
    }

    #[test]
    fn reprices_bn254_and_blake2f_gas() {
        assert_eq!(
            precompile(0x06).execute(&[], u64::MAX).unwrap().gas_used,
            BN254_ADD_GAS
        );
        assert_eq!(
            precompile(0x07).execute(&[], u64::MAX).unwrap().gas_used,
            BN254_MUL_GAS
        );
        assert_eq!(
            precompile(0x08).execute(&[], u64::MAX).unwrap().gas_used,
            BN254_PAIR_BASE_GAS
        );

        let mut blake2_input = [0u8; BLAKE2F_INPUT_LENGTH];
        blake2_input[..4].copy_from_slice(&12u32.to_be_bytes());
        assert_eq!(
            precompile(0x09)
                .execute(&blake2_input, u64::MAX)
                .unwrap()
                .gas_used,
            12 * GFROUND
        );
    }
}
