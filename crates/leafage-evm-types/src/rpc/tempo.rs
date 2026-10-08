use alloy::primitives::{Address, Bytes, FixedBytes, Signature, B256, U256};
use alloy::rpc::types::TransactionRequest;
use serde::{Deserialize, Serialize};

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
    alloy_rlp_derive::RlpDecodable,
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
    alloy_rlp_derive::RlpDecodable,
)]
#[serde(rename_all = "camelCase")]
pub struct SelectorRule {
    pub selector: FixedBytes<4>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<Address>,
}

fn deserialize_signature_type<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(value) = &value {
        if !matches!(
            value.to_ascii_lowercase().as_str(),
            "secp256k1" | "p256" | "webauthn"
        ) {
            return Err(serde::de::Error::custom(format!(
                "unsupported signature type: {value}"
            )));
        }
    }
    Ok(value)
}

mod nonzero_quantity_opt {
    pub use alloy::serde::quantity::opt::serialize;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = alloy::serde::quantity::opt::deserialize(deserializer)?;
        if value == Some(0) {
            return Err(serde::de::Error::custom("expected non-zero quantity"));
        }
        Ok(value)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempoCallExtension {
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "calls")]
    pub tempo_calls: Option<Vec<TransactionRequest>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce_key: Option<U256>,

    #[serde(
        default,
        deserialize_with = "deserialize_signature_type",
        skip_serializing_if = "Option::is_none"
    )]
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

    #[serde(
        default,
        with = "nonzero_quantity_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub valid_after: Option<u64>,

    #[serde(
        default,
        with = "nonzero_quantity_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub valid_before: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempoKeyAuthGasInfo {
    /// Signature type of the key being authorized. `sigType` remains accepted
    /// for backward compatibility with the original gas-only RPC shape.
    #[serde(
        default,
        rename = "keyType",
        alias = "sigType",
        deserialize_with = "deserialize_signature_type",
        skip_serializing_if = "Option::is_none"
    )]
    pub sig_type: Option<String>,

    /// Legacy gas-only limit count. A full authorization derives this from
    /// `limits` instead.
    #[serde(default)]
    pub num_limits: u32,

    #[serde(default, with = "alloy::serde::quantity::opt", skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<Address>,

    #[serde(
        default,
        with = "nonzero_quantity_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub expiry: Option<u64>,

    /// `None` means unlimited spending; `Some([])` denies all spending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Vec<TempoTokenLimitInfo>>,

    /// (T3+, TIP-1011) Per-target call scopes carried on the key authorization.
    /// `None` = unrestricted; `Some([])` = scoped deny-all; `Some([...])` =
    /// listed scopes. Used to derive `ScopeCounts` for `key_auth_gas`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_calls: Option<Vec<CallScope>>,

    /// (T5+, TIP-1053) Optional key-authorization witness. Presence matters;
    /// `bytes32(0)` is still a witness and incurs witness gas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub witness: Option<B256>,

    /// T6 admin-key authorization marker.
    #[serde(default)]
    pub is_admin: bool,

    /// T6 target account binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<Address>,

    /// Full authorization signature. When omitted, the object remains a
    /// gas-only compatibility input and does not mutate AccountKeychain state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<TempoPrimitiveSignatureInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempoTokenLimitInfo {
    pub token: Address,
    pub limit: U256,
    #[serde(default, with = "alloy::serde::quantity")]
    pub period: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TempoPrimitiveSignatureInfo {
    Secp256k1(Signature),
    P256(TempoP256SignatureInfo),
    WebAuthn(TempoWebAuthnSignatureInfo),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempoP256SignatureInfo {
    pub r: B256,
    pub s: B256,
    pub pub_key_x: B256,
    pub pub_key_y: B256,
    pub pre_hash: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempoWebAuthnSignatureInfo {
    pub r: B256,
    pub s: B256,
    pub pub_key_x: B256,
    pub pub_key_y: B256,
    pub webauthn_data: Bytes,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TempoAuthGasInfo {
    #[serde(
        default,
        deserialize_with = "deserialize_signature_type",
        skip_serializing_if = "Option::is_none"
    )]
    pub sig_type: Option<String>,

    #[serde(
        default,
        with = "alloy::serde::quantity::opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub nonce: Option<u64>,

    /// Parsed as the chains-layer TempoSignature by the Tempo RPC adapter.
    /// Kept here as JSON to avoid a types -> chains dependency cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<serde_json::Value>,

    #[serde(default)]
    pub is_keychain: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<Address>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<Address>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<U256>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(info.nonce, Some(5));
    }

    /// TempoKeyAuthGasInfo deserialization.
    #[test]
    fn test_tempo_key_auth_gas_info_deserialization() {
        let json = serde_json::json!({
            "sigType": "webauthn",
            "numLimits": 3,
            "witness": "0x0000000000000000000000000000000000000000000000000000000000000000"
        });

        let info: TempoKeyAuthGasInfo = serde_json::from_value(json).expect("should deserialize");
        assert_eq!(info.sig_type, Some("webauthn".to_string()));
        assert_eq!(info.num_limits, 3);
        assert_eq!(info.witness, Some(B256::ZERO));
    }

    #[test]
    fn test_tempo_full_key_authorization_deserialization() {
        let json = serde_json::json!({
            "chainId": "0x1079",
            "keyType": "p256",
            "keyId": "0x0000000000000000000000000000000000000042",
            "limits": [{
                "token": "0x0000000000000000000000000000000000000001",
                "limit": "0x2a",
                "period": "0x3c"
            }],
            "isAdmin": false,
            "account": "0x0000000000000000000000000000000000000007",
            "signature": {
                "type": "p256",
                "r": "0x0000000000000000000000000000000000000000000000000000000000000001",
                "s": "0x0000000000000000000000000000000000000000000000000000000000000002",
                "pubKeyX": "0x0000000000000000000000000000000000000000000000000000000000000003",
                "pubKeyY": "0x0000000000000000000000000000000000000000000000000000000000000004",
                "preHash": false
            }
        });

        let info: TempoKeyAuthGasInfo = serde_json::from_value(json).expect("should deserialize");
        assert_eq!(info.chain_id, Some(4217));
        assert_eq!(info.sig_type.as_deref(), Some("p256"));
        assert_eq!(info.key_id, Some(Address::with_last_byte(0x42)));
        assert_eq!(info.account, Some(Address::with_last_byte(0x07)));
        assert_eq!(info.limits.as_ref().unwrap()[0].period, 60);
        assert!(matches!(
            info.signature,
            Some(TempoPrimitiveSignatureInfo::P256(_))
        ));
    }

    /// TempoKeyAuthGasInfo defaults when fields are missing.
    #[test]
    fn test_tempo_key_auth_gas_info_defaults() {
        let json = serde_json::json!({});

        let info: TempoKeyAuthGasInfo = serde_json::from_value(json).expect("should deserialize empty");
        assert!(info.sig_type.is_none());
        assert_eq!(info.num_limits, 0);
        assert!(info.witness.is_none());
        assert!(info.signature.is_none());
        assert!(!info.is_admin);
    }
}
