use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::rpc::types::TransactionRequest;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};

// ---------------------------------------------------------------------------
// CallScope / SelectorRule (TIP-1011, T3+)
// ---------------------------------------------------------------------------

/// Per-target call scope. Used in [`TempoKeyAuthGasInfo::allowed_calls`] and
/// (re-exported) in the chains-layer `KeyAuthorization` RLP encoding.
///
/// `selector_rules` semantics: `[]` allows any selector on this target.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize,
    alloy_rlp_derive::RlpEncodable,
)]
#[serde(rename_all = "camelCase")]
pub struct CallScope {
    pub target: Address,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selector_rules: Vec<SelectorRule>,
}

/// Selector-level rule within a `CallScope`.
///
/// `recipients` semantics: `[]` imposes no recipient constraint; otherwise the
/// first ABI address argument must be in the allowlist.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize,
    alloy_rlp_derive::RlpEncodable,
)]
#[serde(rename_all = "camelCase")]
pub struct SelectorRule {
    pub selector: FixedBytes<4>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<Address>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRequest {
    #[serde(flatten)]
    pub inner: TransactionRequest,

    #[serde(flatten)]
    pub tempo: Option<TempoCallExtension>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempoCallExtension {
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "calls")]
    pub tempo_calls: Option<Vec<TransactionRequest>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce_key: Option<U256>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_type: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_data: Option<Bytes>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<Address>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_authorization: Option<TempoKeyAuthGasInfo>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "aaAuthorizationList"
    )]
    pub tempo_authorization_list: Option<Vec<TempoAuthGasInfo>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_token: Option<Address>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_payer: Option<Address>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_payer_signature: Option<alloy::primitives::Signature>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_after: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_before: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempoKeyAuthGasInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig_type: Option<String>,

    #[serde(default)]
    pub num_limits: u32,

    /// (T3+, TIP-1011) Per-target call scopes carried on the key authorization.
    /// `None` = unrestricted; `Some([])` = scoped deny-all; `Some([...])` =
    /// listed scopes. Used to derive `ScopeCounts` for `key_auth_gas`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_calls: Option<Vec<CallScope>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempoAuthGasInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig_type: Option<String>,

    #[serde(default)]
    pub nonce: u64,

    #[serde(default)]
    pub is_keychain: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<Address>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,

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
        let t = req.tempo.as_ref().expect("tempo extension should be present");
        assert_eq!(t.nonce_key, Some(U256::from(1)));
        assert_eq!(t.key_type, Some("p256".to_string()));
        assert_eq!(t.key_data, Some(Bytes::from(vec![0xab, 0xcd])));
        assert_eq!(
            t.key_id,
            Some(Address::with_last_byte(0x03))
        );
        assert_eq!(
            t.fee_token,
            Some(Address::with_last_byte(0x04))
        );
        assert_eq!(
            t.fee_payer,
            Some(Address::with_last_byte(0x05))
        );
        assert_eq!(t.valid_after, Some(1000));
        assert_eq!(t.valid_before, Some(2000));
        assert!(t.tempo_calls.is_some());
        assert_eq!(t.tempo_calls.as_ref().unwrap().len(), 1);

        let auth_list = t.tempo_authorization_list.as_ref().unwrap();
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
        let t = req.tempo.unwrap_or_default();
        assert!(t.tempo_calls.is_none());
        assert!(t.nonce_key.is_none());
        assert!(t.key_type.is_none());
        assert!(t.key_data.is_none());
        assert!(t.key_id.is_none());
        assert!(t.key_authorization.is_none());
        assert!(t.tempo_authorization_list.is_none());
        assert!(t.fee_token.is_none());
        assert!(t.fee_payer.is_none());
        assert!(t.fee_payer_signature.is_none());
        assert!(t.valid_after.is_none());
        assert!(t.valid_before.is_none());
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

        let t1 = req.tempo.as_ref().unwrap();
        let t2 = req2.tempo.as_ref().unwrap();
        assert_eq!(t1.nonce_key, t2.nonce_key);
        assert_eq!(t1.key_type, t2.key_type);
        assert_eq!(t1.fee_token, t2.fee_token);
        assert_eq!(t1.valid_after, t2.valid_after);
        assert_eq!(t1.valid_before, t2.valid_before);
    }
}
