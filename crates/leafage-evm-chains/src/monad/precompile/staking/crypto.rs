//! `staking/util/secp256k1.hpp` and `staking/util/bls.hpp`.

use blst::min_pk::{PublicKey as BlsPublicKey, Signature as BlsSignature};
use blst::BLST_ERROR;
use revm::primitives::{keccak256, Address};
use secp256k1::ecdsa::Signature as SecpSignature;
use secp256k1::{Message, PublicKey as SecpPublicKey, SECP256K1};

/// `BlsSignature::BLS_SIGNATURE_DST`.
const BLS_SIGNATURE_DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

/// `Secp256k1Pubkey(compressed)` + `is_valid()`.
pub(crate) fn secp_pubkey(compressed: &[u8; 33]) -> Option<SecpPublicKey> {
    SecpPublicKey::from_slice(compressed).ok()
}

/// `Secp256k1Signature(compact)` + `is_valid()`.
pub(crate) fn secp_signature(compact: &[u8; 64]) -> Option<SecpSignature> {
    SecpSignature::from_compact(compact).ok()
}

/// `Secp256k1Signature::verify`: ECDSA over the blake3 digest of `message`.
pub(crate) fn secp_verify(
    pubkey: &SecpPublicKey,
    signature: &SecpSignature,
    message: &[u8],
) -> bool {
    let digest: [u8; 32] = blake3::hash(message).into();
    SECP256K1
        .verify_ecdsa(&Message::from_digest(digest), signature, pubkey)
        .is_ok()
}

/// `address_from_secpkey`: keccak of the 64 byte uncompressed point.
pub(crate) fn address_from_secp_pubkey(pubkey: &SecpPublicKey) -> Address {
    let uncompressed = pubkey.serialize_uncompressed();
    Address::from_slice(&keccak256(&uncompressed[1..]).0[12..])
}

/// `BlsPubkey(compressed)` + `is_valid()` (in G1 and not infinity).
pub(crate) fn bls_pubkey(compressed: &[u8; 48]) -> Option<BlsPublicKey> {
    let pubkey = BlsPublicKey::uncompress(compressed).ok()?;
    pubkey.validate().ok()?;
    Some(pubkey)
}

/// `BlsSignature(compressed)` + `is_valid()` (in G2 and not infinity).
pub(crate) fn bls_signature(compressed: &[u8; 96]) -> Option<BlsSignature> {
    let signature = BlsSignature::uncompress(compressed).ok()?;
    signature.validate(true).ok()?;
    Some(signature)
}

/// `BlsSignature::verify`: `blst_core_verify_pk_in_g1` with hash-to-curve.
pub(crate) fn bls_verify(pubkey: &BlsPublicKey, signature: &BlsSignature, message: &[u8]) -> bool {
    signature.verify(true, message, BLS_SIGNATURE_DST, &[], pubkey, true)
        == BLST_ERROR::BLST_SUCCESS
}

/// `address_from_bls_key`: keccak of the 96 byte serialized (uncompressed) key.
pub(crate) fn address_from_bls_pubkey(pubkey: &BlsPublicKey) -> Address {
    Address::from_slice(&keccak256(pubkey.serialize()).0[12..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use blst::min_pk::SecretKey as BlsSecretKey;
    use secp256k1::SecretKey;

    #[test]
    fn secp_round_trip_with_blake3_digest() {
        let sk = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let pk = SecpPublicKey::from_secret_key(SECP256K1, &sk);
        let message = b"monad staking message";
        let digest: [u8; 32] = blake3::hash(message).into();
        let sig = SECP256K1.sign_ecdsa(&Message::from_digest(digest), &sk);

        let parsed_pk = secp_pubkey(&pk.serialize()).unwrap();
        let parsed_sig = secp_signature(&sig.serialize_compact()).unwrap();
        assert!(secp_verify(&parsed_pk, &parsed_sig, message));
        assert!(!secp_verify(&parsed_pk, &parsed_sig, b"other"));
        assert!(secp_pubkey(&[0u8; 33]).is_none());

        // The address is the standard Ethereum derivation of the pubkey.
        let expected =
            revm::primitives::Address::from_raw_public_key(&pk.serialize_uncompressed()[1..]);
        assert_eq!(address_from_secp_pubkey(&parsed_pk), expected);
    }

    #[test]
    fn bls_round_trip_with_pop_dst() {
        let ikm = [9u8; 32];
        let sk = BlsSecretKey::key_gen(&ikm, &[]).unwrap();
        let pk = sk.sk_to_pk();
        let message = b"monad staking message";
        let sig = sk.sign(message, BLS_SIGNATURE_DST, &[]);

        let parsed_pk = bls_pubkey(&pk.compress()).unwrap();
        let parsed_sig = bls_signature(&sig.compress()).unwrap();
        assert!(bls_verify(&parsed_pk, &parsed_sig, message));
        assert!(!bls_verify(&parsed_pk, &parsed_sig, b"other"));
        assert!(bls_pubkey(&[0u8; 48]).is_none());
        // compressed infinity point is rejected
        let mut inf = [0u8; 48];
        inf[0] = 0xc0;
        assert!(bls_pubkey(&inf).is_none());
        assert_eq!(
            address_from_bls_pubkey(&parsed_pk),
            Address::from_slice(&keccak256(pk.serialize()).0[12..])
        );
    }
}
