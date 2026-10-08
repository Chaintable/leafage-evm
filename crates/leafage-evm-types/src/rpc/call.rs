use alloy::rpc::types::TransactionRequest;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};

use super::tempo::TempoCallExtension;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRequest {
    #[serde(flatten)]
    pub inner: TransactionRequest,

    #[serde(flatten, deserialize_with = "deserialize_tempo_extension")]
    pub tempo: Option<TempoCallExtension>,
}

// Deserializing a flattened Option directly swallows malformed Tempo fields.
// Keep the optional API shape, but propagate field errors instead of executing
// the request as an ordinary Ethereum transaction.
fn deserialize_tempo_extension<'de, D>(
    deserializer: D,
) -> Result<Option<TempoCallExtension>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    TempoCallExtension::deserialize(deserializer).map(Some)
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
    use alloy::primitives::{Address, Bytes, U256};

    #[test]
    fn review_zero_validity_and_unknown_signature_types_are_rejected() {
        for field in ["validAfter", "validBefore"] {
            for zero in [serde_json::json!(0), serde_json::json!("0x0")] {
                assert!(
                    serde_json::from_value::<CallRequest>(serde_json::json!({field: zero}))
                        .is_err(),
                    "{field} must be non-zero"
                );
            }
        }
        for request in [
            serde_json::json!({"keyType":"garbage"}),
            serde_json::json!({"keyAuthorization":{"keyType":"garbage"}}),
            serde_json::json!({"aaAuthorizationList":[{"sigType":"garbage"}]}),
            serde_json::json!({"keyAuthorization":{"expiry":"0x0"}}),
            serde_json::json!({"keyAuthorization":{"expiry":0}}),
        ] {
            assert!(
                serde_json::from_value::<CallRequest>(request.clone()).is_err(),
                "{request}"
            );
        }
        for key_type in ["secp256k1", "p256", "webAuthn", "P256"] {
            assert!(serde_json::from_value::<CallRequest>(
                serde_json::json!({"keyType":key_type,"validAfter":null})
            )
            .is_ok());
        }
    }

    #[test]
    fn review_tempo_quantity_and_invalid_field_deserialization() {
        for (after, before) in [
            (serde_json::json!(100), serde_json::json!(200)),
            (serde_json::json!("0x64"), serde_json::json!("0xc8")),
        ] {
            let request: CallRequest = serde_json::from_value(
                serde_json::json!({"nonceKey":"0x0", "validAfter":after, "validBefore":before}),
            )
            .unwrap();
            let tempo = request.tempo.expect("Tempo fields must not be discarded");
            assert_eq!(tempo.valid_after, Some(100));
            assert_eq!(tempo.valid_before, Some(200));
        }
        for invalid in [
            serde_json::json!({"validAfter":"bad"}),
            serde_json::json!({"nonceKey":"bad"}),
            serde_json::json!({"feeToken":"bad"}),
        ] {
            assert!(serde_json::from_value::<CallRequest>(invalid).is_err());
        }
    }

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
