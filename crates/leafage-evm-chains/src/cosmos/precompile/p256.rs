use alloy::primitives::address;
use num_bigint::BigUint;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::elliptic_curve::sec1::FromEncodedPoint;
use p256::elliptic_curve::PrimeField;
use p256::{AffinePoint, EncodedPoint, FieldBytes, FieldElement, PublicKey, Scalar};
use revm::precompile::PrecompileWithAddress;
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};

pub const P256_VERIFY: PrecompileWithAddress = PrecompileWithAddress(
    address!("0x0000000000000000000000000000000000000100"),
    secp256r1_signature_verification_run,
);

const VERIFY_GAS: u64 = 3_450;
const INPUT_LENGTH: usize = 160;

/// Run executes the p256 signature verification using ECDSA.
///
/// Input data: 160 bytes of data including:
///   - 32 bytes of the signed data hash
///   - 32 bytes of the r component of the signature
///   - 32 bytes of the s component of the signature
///   - 32 bytes of the x coordinate of the public key
///   - 32 bytes of the y coordinate of the public key
///
/// Output data: 32 bytes of result data and error
///   - If the signature verification process succeeds, it returns 1 in 32 bytes format
fn secp256r1_signature_verification_run(input: &[u8], gas_limit: u64) -> PrecompileResult {
    if gas_limit < VERIFY_GAS {
        return Err(PrecompileError::OutOfGas);
    }
    if input.len() != INPUT_LENGTH {
        return Err(PrecompileError::other("invalid input"));
    }
    let hash = &input[..32];
    let r = &input[32..64];
    let s = &input[64..96];
    let x = &input[96..128];
    let y = &input[128..160];
    let Some(r) = Scalar::from_repr_vartime(FieldBytes::clone_from_slice(r)) else {
        return Err(PrecompileError::other("invalid r"));
    };
    let Some(s) = Scalar::from_repr_vartime(FieldBytes::clone_from_slice(s)) else {
        return Err(PrecompileError::other("invalid s"));
    };
    let Ok(x) = FieldElement::from_slice(x) else {
        return Err(PrecompileError::other("invalid x"));
    };
    let Ok(y) = FieldElement::from_slice(y) else {
        return Err(PrecompileError::other("invalid y"));
    };
    secp256r1_verify(hash, r, s, x, y)
}

fn secp256r1_verify(
    hash: &[u8],
    r: Scalar,
    s: Scalar,
    x: FieldElement,
    y: FieldElement,
) -> PrecompileResult {
    // parse publicKey
    let encoded_point = EncodedPoint::from_affine_coordinates(&x.to_repr(), &y.to_repr(), false);
    let Some(affine_point): Option<_> = AffinePoint::from_encoded_point(&encoded_point).into()
    else {
        return Err(PrecompileError::other("invalid x or y"));
    };
    let Ok(public_key) = PublicKey::from_affine(affine_point) else {
        return Err(PrecompileError::other("invalid pubkey"));
    };
    let verifying_key = VerifyingKey::from(public_key);

    // generate signature
    let Ok(signature) = Signature::from_scalars(r, s) else {
        return Err(PrecompileError::other("invalid signature"));
    };

    // verify signature
    let padding = match verifying_key.verify(hash, &signature) {
        Ok(_) => {
            let big1 = BigUint::from(1u8);
            let big1_bytes = big1.to_bytes_be();
            let mut padded = vec![0u8; 32];
            let start = 32 - big1_bytes.len();
            padded[start..].copy_from_slice(&big1_bytes);
            padded
        }
        Err(_) => Default::default(),
    };
    Ok(PrecompileOutput::new(VERIFY_GAS, padding.into()))
}
