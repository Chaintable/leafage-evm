use alloy::primitives::address;
use revm::precompile::{
    Precompile, PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult,
};
use revm::primitives::Bytes;
use std::borrow::Cow;

// 4600 ~ 1.33 * 3450 (p256r1 base gas cost), reflecting ~33% higher zk cycle count for schnorr
const SCHNORRVERIFY_BASE: u64 = 4600;
const INPUT_LENGTH: usize = 128;
pub const SCHNORR_VERIFY: Precompile = Precompile::new(
    PrecompileId::Custom(Cow::Borrowed("CITREA_SCHNORR_VERIFY")),
    address!("0x0000000000000000000000000000000000000200"),
    schnorr_verify_run,
);

/// BIP340 Schnorr verify. Input: pubkey_x(32) | msg_hash(32) | sig(64) = 128 bytes.
/// Returns 32-byte big-endian 1 on success, empty bytes on failure.
fn schnorr_verify_run(input: &[u8], gas_limit: u64) -> PrecompileResult {
    if gas_limit < SCHNORRVERIFY_BASE {
        return Err(PrecompileError::OutOfGas);
    }

    let result = verify_sig(input).map_or_else(Bytes::new, |_| {
        let mut out = [0u8; 32];
        out[31] = 1;
        Bytes::from(out.to_vec())
    });

    Ok(PrecompileOutput::new(SCHNORRVERIFY_BASE, result))
}

fn verify_sig(input: &[u8]) -> Option<()> {
    use k256::schnorr::signature::hazmat::PrehashVerifier;
    use k256::schnorr::{Signature, VerifyingKey};

    if input.len() != INPUT_LENGTH {
        return None;
    }
    let verifying_key = VerifyingKey::from_bytes(&input[..32]).ok()?;
    let message = &input[32..64];
    let signature = Signature::try_from(&input[64..]).ok()?;
    verifying_key.verify_prehash(message, &signature).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insufficient_gas() {
        let input = [0u8; 128];
        assert_eq!(
            schnorr_verify_run(&input, SCHNORRVERIFY_BASE - 1),
            Err(PrecompileError::OutOfGas)
        );
    }

    #[test]
    fn test_invalid_input_length() {
        let result = schnorr_verify_run(&[0u8; 127], SCHNORRVERIFY_BASE);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().bytes, Bytes::new());

        let result = schnorr_verify_run(&[0u8; 129], SCHNORRVERIFY_BASE);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().bytes, Bytes::new());
    }

    #[test]
    fn test_invalid_signature_returns_empty() {
        let result = schnorr_verify_run(&[0u8; 128], SCHNORRVERIFY_BASE);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().bytes, Bytes::new());
    }
}
