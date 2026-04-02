use alloy::primitives::{Address, Bytes, U256};
use alloy::rpc::types::TransactionRequest;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};

/// Extended call request wrapping alloy's `TransactionRequest` with Tempo-specific fields.
///
/// Uses `Deref`/`DerefMut` to `TransactionRequest` so all existing field access
/// (e.g., `request.to`, `request.input`, `request.nonce`) works without changes.
///
/// Gas estimation fields (`key_type`, `key_data`, `key_id`, `key_authorization`,
/// `tempo_authorization_list`) mirror Tempo writer's `TempoTransactionRequest`
/// to ensure gas calculation parity for AA transactions.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRequest {
    #[serde(flatten)]
    pub inner: TransactionRequest,

    /// Tempo batch calls for AA tx (type 0x76).
    /// Each call in the batch is executed atomically.
    /// Writer field name: `calls` (camelCase identity).
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "calls")]
    pub tempo_calls: Option<Vec<TransactionRequest>>,

    /// Tempo 2D nonce key. When non-zero, nonce is read from NonceManager precompile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce_key: Option<U256>,

    // --- AA gas estimation fields (match writer's TempoTransactionRequest) ---

    /// Signature type for gas estimation: "secp256k1" (default), "p256", "webauthn".
    /// Determines additional signature verification gas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_type: Option<String>,

    /// Key-specific data for gas estimation (e.g., WebAuthn authenticator data size).
    /// When `key_type` is "webauthn", this encodes the total size (1/2/4 bytes BE).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_data: Option<Bytes>,

    /// Access key ID for gas estimation.
    /// When present, indicates the transaction uses a Keychain (access key) signature,
    /// adding KEYCHAIN_VALIDATION_GAS (~3k) to intrinsic gas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<Address>,

    /// Key authorization for provisioning an access key (gas estimation).
    /// Lightweight representation: only signature type and spending limits count matter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_authorization: Option<TempoKeyAuthGasInfo>,

    /// Authorization list for Tempo transactions (supports multiple signature types).
    /// Each entry's signature type and nonce affect gas calculation.
    /// Optionally includes delegation fields (authority/address) for EIP-7702 application.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "aaAuthorizationList"
    )]
    pub tempo_authorization_list: Option<Vec<TempoAuthGasInfo>>,

    // --- Tempo transaction fields (match writer's TempoTransactionRequest) ---

    /// Fee token address for gas payment. When specified, overrides the stored
    /// fee token preference. Affects estimateGas gas cap calculation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_token: Option<Address>,

    /// Fee payer (sponsor) address. When specified directly, the sponsor's fee token
    /// balance is used for the estimateGas gas cap instead of the caller's.
    /// If `feePayerSignature` is also present, the recovered address takes priority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_payer: Option<Address>,

    /// Fee payer signature for sponsored transactions (matches writer wire format).
    /// When present, leafage recovers the sponsor address via ecrecover over the
    /// fee_payer_signature_hash of the full transaction fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_payer_signature: Option<alloy::primitives::Signature>,

    /// Earliest block timestamp at which this AA transaction is valid (seconds).
    /// If block_timestamp < valid_after, eth_call/estimateGas returns an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_after: Option<u64>,

    /// Latest block timestamp before which this AA transaction is valid (seconds).
    /// If block_timestamp >= valid_before, eth_call/estimateGas returns an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_before: Option<u64>,
}

/// Lightweight key authorization info for gas estimation.
///
/// Full `SignedKeyAuthorization` has complex nested types; this captures
/// only the fields that affect gas: the signature type and limits count.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempoKeyAuthGasInfo {
    /// Signature type on the key authorization ("secp256k1", "p256", "webauthn").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig_type: Option<String>,

    /// Number of spending limits. Each limit costs KEY_AUTH_PER_LIMIT_GAS (22k pre-T1B)
    /// or SSTORE_SET (250k post-T1B) in gas.
    #[serde(default)]
    pub num_limits: u32,
}

/// Per-authorization info for gas estimation and optional EIP-7702 delegation.
///
/// Lightweight fields (`sig_type`, `nonce`, `is_keychain`) are always used for gas calculation.
/// Optional delegation fields (`authority`, `address`, `chain_id`) enable EIP-7702
/// code delegation in eth_call when provided.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempoAuthGasInfo {
    /// Signature type ("secp256k1", "p256", "webauthn").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig_type: Option<String>,

    /// Nonce of the authorization. When 0, incurs TIP-1000 account creation cost.
    #[serde(default)]
    pub nonce: u64,

    /// Whether this authorization uses a Keychain (access key) signature.
    /// When true, adds KEYCHAIN_VALIDATION_GAS (3000) to per-auth gas.
    #[serde(default)]
    pub is_keychain: bool,

    // --- EIP-7702 delegation fields (optional, for apply_eip7702_auth_list) ---

    /// Authority address — the EOA whose code gets delegated.
    /// In the writer, this is recovered from signature. For RPC, provided directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<Address>,

    /// Delegate address — the contract to set as delegation target.
    /// Sets authority's code to `0xef0100 || address` in the journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,

    /// Chain ID for the authorization (0 = any chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<U256>,
}

impl Deref for CallRequest {
    type Target = TransactionRequest;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for CallRequest {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify camelCase deserialization of all Tempo-specific fields.
    #[test]
    fn test_call_request_camel_case_deserialization() {
        let json = serde_json::json!({
            "from": "0x0000000000000000000000000000000000000001",
            "to": "0x0000000000000000000000000000000000000002",
            "gas": "0x100000",
            "nonceKey": "0x1",
            "keyType": "p256",
            "keyData": "0xabcd",
            "keyId": "0x0000000000000000000000000000000000000003",
            "feeToken": "0x0000000000000000000000000000000000000004",
            "feePayer": "0x0000000000000000000000000000000000000005",
            "validAfter": 1000,
            "validBefore": 2000,
            "calls": [
                { "from": "0x0000000000000000000000000000000000000001", "to": "0x0000000000000000000000000000000000000002" }
            ],
            "aaAuthorizationList": [
                {
                    "isKeychain": true,
                    "authority": "0x0000000000000000000000000000000000000006",
                    "address": "0x0000000000000000000000000000000000000007",
                    "chainId": "0x1"
                }
            ]
        });

        let req: CallRequest = serde_json::from_value(json).expect("should deserialize");
        assert_eq!(req.nonce_key, Some(U256::from(1)));
        assert_eq!(req.key_type, Some("p256".to_string()));
        assert_eq!(req.key_data, Some(Bytes::from(vec![0xab, 0xcd])));
        assert_eq!(
            req.key_id,
            Some(Address::with_last_byte(0x03))
        );
        assert_eq!(
            req.fee_token,
            Some(Address::with_last_byte(0x04))
        );
        assert_eq!(
            req.fee_payer,
            Some(Address::with_last_byte(0x05))
        );
        assert_eq!(req.valid_after, Some(1000));
        assert_eq!(req.valid_before, Some(2000));
        assert!(req.tempo_calls.is_some());
        assert_eq!(req.tempo_calls.as_ref().unwrap().len(), 1);

        let auth_list = req.tempo_authorization_list.as_ref().unwrap();
        assert_eq!(auth_list.len(), 1);
        assert!(auth_list[0].is_keychain);
        assert_eq!(
            auth_list[0].authority,
            Some(Address::with_last_byte(0x06))
        );
        assert_eq!(
            auth_list[0].address,
            Some(Address::with_last_byte(0x07))
        );
        assert_eq!(auth_list[0].chain_id, Some(U256::from(1)));
    }

    /// Standard eth_call request WITHOUT any Tempo-specific fields.
    /// All Tempo extensions should be None/default.
    #[test]
    fn test_call_request_backwards_compatible() {
        let json = serde_json::json!({
            "from": "0x0000000000000000000000000000000000000001",
            "to": "0x0000000000000000000000000000000000000002",
            "gas": "0x5208",
            "value": "0x0",
            "input": "0x"
        });

        let req: CallRequest = serde_json::from_value(json).expect("should deserialize standard request");
        assert!(req.tempo_calls.is_none(), "tempo_calls should be None");
        assert!(req.nonce_key.is_none(), "nonce_key should be None");
        assert!(req.key_type.is_none(), "key_type should be None");
        assert!(req.key_data.is_none(), "key_data should be None");
        assert!(req.key_id.is_none(), "key_id should be None");
        assert!(req.key_authorization.is_none(), "key_authorization should be None");
        assert!(req.tempo_authorization_list.is_none(), "tempo_authorization_list should be None");
        assert!(req.fee_token.is_none(), "fee_token should be None");
        assert!(req.fee_payer.is_none(), "fee_payer should be None");
        assert!(req.fee_payer_signature.is_none(), "fee_payer_signature should be None");
        assert!(req.valid_after.is_none(), "valid_after should be None");
        assert!(req.valid_before.is_none(), "valid_before should be None");
    }

    /// TempoAuthGasInfo deserialization with camelCase fields.
    #[test]
    fn test_tempo_auth_gas_info_deserialization() {
        let json = serde_json::json!({
            "isKeychain": true,
            "authority": "0x0000000000000000000000000000000000000001",
            "address": "0x0000000000000000000000000000000000000002",
            "chainId": "0x1077",
            "sigType": "p256",
            "nonce": 5
        });

        let info: TempoAuthGasInfo = serde_json::from_value(json).expect("should deserialize");
        assert!(info.is_keychain);
        assert_eq!(info.authority, Some(Address::with_last_byte(0x01)));
        assert_eq!(info.address, Some(Address::with_last_byte(0x02)));
        assert_eq!(info.chain_id, Some(U256::from(0x1077)));
        assert_eq!(info.sig_type, Some("p256".to_string()));
        assert_eq!(info.nonce, 5);
    }

    /// TempoKeyAuthGasInfo deserialization.
    #[test]
    fn test_tempo_key_auth_gas_info_deserialization() {
        let json = serde_json::json!({
            "sigType": "webauthn",
            "numLimits": 3
        });

        let info: TempoKeyAuthGasInfo = serde_json::from_value(json).expect("should deserialize");
        assert_eq!(info.sig_type, Some("webauthn".to_string()));
        assert_eq!(info.num_limits, 3);
    }

    /// TempoKeyAuthGasInfo defaults when fields are missing.
    #[test]
    fn test_tempo_key_auth_gas_info_defaults() {
        let json = serde_json::json!({});

        let info: TempoKeyAuthGasInfo = serde_json::from_value(json).expect("should deserialize empty");
        assert!(info.sig_type.is_none());
        assert_eq!(info.num_limits, 0);
    }

    /// CallRequest serialization round-trip: serialize then deserialize.
    #[test]
    fn test_call_request_serde_round_trip() {
        let json = serde_json::json!({
            "from": "0x0000000000000000000000000000000000000001",
            "to": "0x0000000000000000000000000000000000000002",
            "nonceKey": "0x42",
            "keyType": "secp256k1",
            "feeToken": "0x0000000000000000000000000000000000000099",
            "validAfter": 100,
            "validBefore": 200
        });

        let req: CallRequest = serde_json::from_value(json).expect("deserialize");
        let serialized = serde_json::to_value(&req).expect("serialize");
        let req2: CallRequest = serde_json::from_value(serialized).expect("re-deserialize");

        assert_eq!(req.nonce_key, req2.nonce_key);
        assert_eq!(req.key_type, req2.key_type);
        assert_eq!(req.fee_token, req2.fee_token);
        assert_eq!(req.valid_after, req2.valid_after);
        assert_eq!(req.valid_before, req2.valid_before);
    }
}
