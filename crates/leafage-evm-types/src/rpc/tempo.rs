use alloy::primitives::{Address, Bytes, FixedBytes, Signature, B256, U256};
use alloy::rpc::types::TransactionRequest;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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

    /// Set when the Tempo fields are present but malformed. All other fields
    /// are then left empty, and only the Tempo adapter rejects the request.
    #[serde(skip)]
    pub invalid: Option<InvalidTempoFields>,
}

/// Malformed Tempo fields of a [`super::CallRequest`]. The raw values are
/// serialized unchanged, so a request forwarded to a historical node is still
/// rejected there instead of running as an ordinary transaction.
#[derive(Clone, Debug)]
pub struct InvalidTempoFields {
    pub error: String,
    raw: Map<String, Value>,
}

/// JSON names of the [`TempoCallExtension`] fields; the test
/// `tempo_field_names_match_extension` keeps the two in sync.
const TEMPO_FIELD_NAMES: &[&str] = &[
    "calls",
    "nonceKey",
    "keyType",
    "keyData",
    "keyId",
    "keyAuthorization",
    "aaAuthorizationList",
    "feeToken",
    "feePayer",
    "feePayerSignature",
    "validAfter",
    "validBefore",
];

/// Collects only the Tempo-named entries of the flattened request, so other
/// chains do not copy their own fields.
struct TempoFieldsVisitor;

impl<'de> serde::de::Visitor<'de> for TempoFieldsVisitor {
    type Value = Map<String, Value>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a call request object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut fields = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if TEMPO_FIELD_NAMES.contains(&key.as_str()) {
                fields.insert(key, map.next_value()?);
            } else {
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        Ok(fields)
    }
}

pub(super) fn deserialize_call_extension<'de, D>(
    deserializer: D,
) -> Result<Option<TempoCallExtension>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let fields = Value::Object(deserializer.deserialize_map(TempoFieldsVisitor)?);
    let extension = match TempoCallExtension::deserialize(&fields) {
        Ok(extension) => extension,
        Err(error) => {
            let Value::Object(raw) = fields else {
                unreachable!("Tempo fields are collected into an object")
            };
            TempoCallExtension {
                invalid: Some(InvalidTempoFields {
                    error: error.to_string(),
                    raw,
                }),
                ..Default::default()
            }
        }
    };
    Ok(Some(extension))
}

pub(super) fn serialize_call_extension<S>(
    extension: &Option<TempoCallExtension>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match extension
        .as_ref()
        .and_then(|extension| extension.invalid.as_ref())
    {
        Some(invalid) => invalid.raw.serialize(serializer),
        None => extension.serialize(serializer),
    }
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

    /// Every serialized extension field must be listed in TEMPO_FIELD_NAMES,
    /// otherwise a malformed value in it would be dropped instead of rejected.
    #[test]
    fn tempo_field_names_match_extension() {
        let extension = TempoCallExtension {
            tempo_calls: Some(vec![]),
            nonce_key: Some(U256::ZERO),
            key_type: Some("p256".to_string()),
            key_data: Some(Bytes::new()),
            key_id: Some(Address::ZERO),
            key_authorization: Some(TempoKeyAuthGasInfo::default()),
            tempo_authorization_list: Some(vec![]),
            fee_token: Some(Address::ZERO),
            fee_payer: Some(Address::ZERO),
            fee_payer_signature: Some(Signature::new(U256::ONE, U256::ONE, false)),
            valid_after: Some(1),
            valid_before: Some(2),
            invalid: None,
        };
        let serialized = serde_json::to_value(extension).unwrap();
        let mut names: Vec<_> = serialized.as_object().unwrap().keys().cloned().collect();
        let mut expected: Vec<_> = TEMPO_FIELD_NAMES
            .iter()
            .map(|name| name.to_string())
            .collect();
        names.sort();
        expected.sort();
        assert_eq!(names, expected);
    }
}
