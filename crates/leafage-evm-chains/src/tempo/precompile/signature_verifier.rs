//! TIP-1020 signature verifier precompile (T3+).
//!
//! Ported from Tempo writer `crates/precompiles/src/signature_verifier/`.
//!
//! Stateless precompile that exposes `recover(hash, sig)` and
//! `verify(signer, hash, sig)` to contracts. All real parsing and verification
//! work lives in [`PrimitiveSignature::from_bytes`] and
//! [`PrimitiveSignature::recover_signer`] in `tempo::fee_payer`.
//!
//! Gas:
//! - Secp256k1: 3_000
//! - P256:      8_000
//! - WebAuthn:  8_000
//!
//! Activation: T3+. Registration is gated in [`extend_tempo_precompiles`]; this
//! file additionally guards the dispatch entry as defense in depth.

use alloy::primitives::{Address, Bytes, B256};
use alloy::sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileResult};

use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx};
use super::{dispatch_call, input_cost, unknown_selector, view, Precompile, SIGNATURE_VERIFIER_ADDRESS};
use crate::tempo::fee_payer::PrimitiveSignature;

// ===========================================================================
// Gas constants
// ===========================================================================

const SECP256K1_VERIFY_GAS: u64 = 3_000;
const P256_VERIFY_GAS: u64 = 8_000;
const WEBAUTHN_VERIFY_GAS: u64 = 8_000;

/// Max WebAuthn signature payload (mirrors `tempo::fee_payer::MAX_WEBAUTHN_SIGNATURE_LENGTH`).
const MAX_WEBAUTHN_SIGNATURE_LENGTH: usize = 2048;

/// Upper bound on calldata size. ABI-encoded `verify(addr,bytes32,bytes)` with a
/// max-size WebAuthn signature is the worst case: selector(4) + 4 args × 32 bytes
/// + dynamic `bytes` field padded to a 32-byte multiple.
const MAX_CALLDATA_LEN: usize =
    4 + 32 * 4 + (MAX_WEBAUTHN_SIGNATURE_LENGTH + 1).next_multiple_of(32);

// ===========================================================================
// Solidity ABI
// ===========================================================================

alloy::sol! {
    interface ISignatureVerifier {
        /// Recovers the signer of a Tempo signature (secp256k1, P256, WebAuthn).
        function recover(bytes32 hash, bytes calldata signature) external view returns (address signer);

        /// Verifies a signer against a Tempo signature.
        function verify(address signer, bytes32 hash, bytes calldata signature) external view returns (bool);

        error InvalidFormat();
        error InvalidSignature();
    }
}

// ===========================================================================
// SignatureVerifier struct
// ===========================================================================

/// Stateless TIP-1020 signature verifier precompile.
pub struct SignatureVerifier {
    pub address: Address,
    pub storage: StorageCtx,
}

impl SignatureVerifier {
    pub fn new() -> Self {
        Self {
            address: SIGNATURE_VERIFIER_ADDRESS,
            storage: StorageCtx::default(),
        }
    }

    /// Verify the signature and recover the signer address.
    pub fn recover(&mut self, hash: B256, signature: Bytes) -> Result<Address> {
        let sig = PrimitiveSignature::from_bytes(&signature).map_err(|_| {
            TempoPrecompileError::Revert(
                ISignatureVerifier::InvalidFormat {}.abi_encode().into(),
            )
        })?;

        // Charge per-scheme verification gas before doing the work.
        let verify_gas = match sig {
            PrimitiveSignature::Secp256k1(_) => SECP256K1_VERIFY_GAS,
            PrimitiveSignature::P256(_) => P256_VERIFY_GAS,
            PrimitiveSignature::WebAuthn(_) => WEBAUTHN_VERIFY_GAS,
        };
        self.storage
            .deduct_gas(verify_gas)
            .map_err(|_| TempoPrecompileError::OutOfGas)?;

        sig.recover_signer(&hash).map_err(|_| {
            TempoPrecompileError::Revert(
                ISignatureVerifier::InvalidSignature {}.abi_encode().into(),
            )
        })
    }

    /// Returns `true` if `signature` is a valid signature by `signer` over `hash`.
    /// Reverts on signature parse errors (matches `recover`); returns `false` only
    /// when the signature parses and recovers a different signer.
    pub fn verify(&mut self, signer: Address, hash: B256, signature: Bytes) -> Result<bool> {
        let recovered = self.recover(hash, signature)?;
        Ok(recovered == signer)
    }
}

impl ContractStorage for SignatureVerifier {
    #[inline]
    fn address(&self) -> Address {
        self.address
    }

    #[inline]
    fn storage(&self) -> &StorageCtx {
        &self.storage
    }

    #[inline]
    fn storage_mut(&mut self) -> &mut StorageCtx {
        &mut self.storage
    }
}

// ===========================================================================
// Dispatch
// ===========================================================================

impl Precompile for SignatureVerifier {
    fn call(&mut self, calldata: &[u8], _msg_sender: Address) -> PrecompileResult {
        // Defense in depth: registration in extend_tempo_precompiles already gates
        // on spec.is_t3(), but reject again at dispatch time to catch any caller
        // that bypassed the lookup (e.g. tests).
        if !self.storage.spec().is_t3() {
            let selector: [u8; 4] = if calldata.len() >= 4 {
                calldata[..4].try_into().expect("4-byte slice")
            } else {
                [0; 4]
            };
            return unknown_selector(selector, 0);
        }

        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        if calldata.len() > MAX_CALLDATA_LEN {
            return Err(PrecompileError::other("calldata exceeds MAX_CALLDATA_LEN"));
        }

        dispatch_call(
            calldata,
            ISignatureVerifier::ISignatureVerifierCalls::abi_decode,
            |call| match call {
                ISignatureVerifier::ISignatureVerifierCalls::recover(c) => {
                    view(c, |c| self.recover(c.hash, c.signature))
                }
                ISignatureVerifier::ISignatureVerifierCalls::verify(c) => {
                    view(c, |c| self.verify(c.signer, c.hash, c.signature))
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempo::fee_payer::P256SignatureWithPreHash;
    use crate::tempo::hardfork::TempoHardfork;
    use crate::tempo::precompile::storage::with_read_only_storage_ctx;
    use alloy::primitives::Signature;
    use alloy::sol_types::SolCall;
    use revm::database::EmptyDB;

    fn run_with_spec<F: FnOnce() -> R, R>(spec: TempoHardfork, f: F) -> R {
        // Stateless precompile — EmptyDB is fine.
        with_read_only_storage_ctx(&EmptyDB::default(), spec, 4217, f)
    }

    #[test]
    fn pre_t3_call_returns_unknown_selector() {
        let calldata = ISignatureVerifier::recoverCall {
            hash: B256::ZERO,
            signature: Bytes::from(vec![0u8; 65]),
        }
        .abi_encode();

        let result = run_with_spec(TempoHardfork::T2, || {
            SignatureVerifier::new().call(&calldata, Address::ZERO)
        });

        let output = result.expect("precompile call returns Ok");
        assert!(output.reverted, "pre-T3 must revert");
    }

    #[test]
    fn t3_recover_secp256k1_parses_test_signature() {
        // `Signature::test_signature()` is a placeholder with r=1, s=1, v=0 — it
        // parses as a well-formed secp256k1 signature but does not recover any
        // real signer. We just verify the precompile accepts the 65-byte wire
        // format and reports a deterministic outcome (no panic, no OOG).
        let sig = PrimitiveSignature::Secp256k1(Signature::test_signature()).to_bytes();
        let calldata = ISignatureVerifier::recoverCall {
            hash: B256::from([0xAA; 32]),
            signature: sig,
        }
        .abi_encode();

        let result = run_with_spec(TempoHardfork::T3, || {
            SignatureVerifier::new().call(&calldata, Address::ZERO)
        });

        // Must not panic / OOG. Outcome is either a valid recovery or an
        // InvalidSignature revert — both are acceptable.
        let _output = result.expect("precompile call returns Ok");
    }

    #[test]
    fn t3_oversized_calldata_rejected() {
        let calldata = vec![0u8; MAX_CALLDATA_LEN + 1];

        let result = run_with_spec(TempoHardfork::T3, || {
            SignatureVerifier::new().call(&calldata, Address::ZERO)
        });

        // Returns Err(PrecompileError::Other) at the size guard.
        assert!(result.is_err());
    }

    #[test]
    fn t3_invalid_format_signature_reverts() {
        let calldata = ISignatureVerifier::recoverCall {
            hash: B256::ZERO,
            signature: Bytes::from(vec![0u8; 13]), // not 65, not a valid type-prefixed length
        }
        .abi_encode();

        let result = run_with_spec(TempoHardfork::T3, || {
            SignatureVerifier::new().call(&calldata, Address::ZERO)
        });

        let output = result.expect("precompile call returns Ok");
        assert!(output.reverted, "invalid sig format must revert");
        // Revert data should be the InvalidFormat selector.
        let invalid_format_selector = ISignatureVerifier::InvalidFormat::SELECTOR;
        assert_eq!(&output.bytes[..4], &invalid_format_selector);
    }

    #[test]
    fn t3_short_calldata_revert() {
        // <4 bytes -> dispatch_call returns generic revert.
        let result = run_with_spec(TempoHardfork::T3, || {
            SignatureVerifier::new().call(&[0u8; 3], Address::ZERO)
        });
        let output = result.expect("precompile call returns Ok");
        assert!(output.reverted);
    }

    #[test]
    fn t3_p256_verify_round_trip() {
        use p256::ecdsa::{
            signature::hazmat::PrehashSigner, Signature as P256Signature, SigningKey,
        };
        use p256::elliptic_curve::rand_core::OsRng;

        let signing_key = SigningKey::random(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let encoded = verifying_key.to_encoded_point(false);
        let pub_key_x = B256::from_slice(encoded.x().unwrap());
        let pub_key_y = B256::from_slice(encoded.y().unwrap());

        let hash = B256::from([0xBB; 32]);
        let sig: P256Signature = signing_key.sign_prehash(hash.as_slice()).unwrap();
        let normalized = sig.normalize_s().unwrap_or(sig);
        let r = B256::from_slice(&normalized.r().to_bytes());
        let s = B256::from_slice(&normalized.s().to_bytes());

        let primitive = PrimitiveSignature::P256(P256SignatureWithPreHash {
            r,
            s,
            pub_key_x,
            pub_key_y,
            pre_hash: false,
        });
        let expected_signer = primitive.recover_signer(&hash).expect("self-recover");

        let calldata = ISignatureVerifier::verifyCall {
            signer: expected_signer,
            hash,
            signature: primitive.to_bytes(),
        }
        .abi_encode();

        let result = run_with_spec(TempoHardfork::T3, || {
            SignatureVerifier::new().call(&calldata, Address::ZERO)
        });
        let output = result.expect("precompile call ok");
        assert!(!output.reverted, "verify with correct signer must succeed");

        let returns = ISignatureVerifier::verifyCall::abi_decode_returns(&output.bytes)
            .expect("decode verify return");
        assert!(returns, "verify must return true for correct signer");
    }
}
