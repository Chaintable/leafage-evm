//! Fee payer signature hash computation and recovery for Tempo transactions.
//!
//! Ported from `tempo_primitives::transaction`:
//! - `tempo_transaction.rs` — `Call`, `fee_payer_signature_hash`, `rlp_encode_fields`, `rlp_encoded_fields_length`, `FEE_PAYER_SIGNATURE_MAGIC_BYTE`
//! - `tt_signature.rs` — `TempoSignature`, `PrimitiveSignature`, `KeychainSignature`, `SignatureType`
//! - `tt_authorization.rs` — `TempoSignedAuthorization`
//! - `key_authorization.rs` — `KeyAuthorization`, `SignedKeyAuthorization`, `TokenLimit`
//!
//! Only includes types and logic needed for `fee_payer_signature_hash` + `recover_fee_payer`.
//! Decodable, Compact, arbitrary impls are intentionally omitted.

use alloy::eips::{eip2930::AccessList, eip7702::Authorization};
use alloy::primitives::{Address, Bytes, Signature, B256, U256, keccak256};
use alloy_rlp::{BufMut, Encodable, Header, EMPTY_STRING_CODE, encode_list, length_of_length, list_length};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Magic byte prefix for the fee payer signature hash.
pub const FEE_PAYER_SIGNATURE_MAGIC_BYTE: u8 = 0x78;

/// Secp256k1 signature wire length (no type prefix).
const SECP256K1_SIGNATURE_LENGTH: usize = 65;

/// P256 signature wire length (excluding the 1-byte type prefix).
const P256_SIGNATURE_LENGTH: usize = 129;

/// Signature type prefix bytes.
const SIGNATURE_TYPE_P256: u8 = 0x01;
const SIGNATURE_TYPE_WEBAUTHN: u8 = 0x02;
const SIGNATURE_TYPE_KEYCHAIN: u8 = 0x03;
const SIGNATURE_TYPE_KEYCHAIN_V2: u8 = 0x04;

// ---------------------------------------------------------------------------
// SignatureType
// ---------------------------------------------------------------------------

/// Signature algorithm type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[repr(u8)]
pub enum SignatureType {
    Secp256k1 = 0,
    P256 = 1,
    WebAuthn = 2,
}

impl From<SignatureType> for u8 {
    fn from(sig_type: SignatureType) -> Self {
        sig_type as u8
    }
}

impl Encodable for SignatureType {
    fn encode(&self, out: &mut dyn BufMut) {
        (*self as u8).encode(out);
    }

    fn length(&self) -> usize {
        1
    }
}

// ---------------------------------------------------------------------------
// P256SignatureWithPreHash
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct P256SignatureWithPreHash {
    pub r: B256,
    pub s: B256,
    pub pub_key_x: B256,
    pub pub_key_y: B256,
    pub pre_hash: bool,
}

// ---------------------------------------------------------------------------
// WebAuthnSignature
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebAuthnSignature {
    pub r: B256,
    pub s: B256,
    pub pub_key_x: B256,
    pub pub_key_y: B256,
    /// authenticatorData || clientDataJSON (variable length)
    pub webauthn_data: Bytes,
}

// ---------------------------------------------------------------------------
// PrimitiveSignature (non-recursive base signatures)
// ---------------------------------------------------------------------------

/// Base signature types: Secp256k1, P256, WebAuthn.
/// Does NOT include Keychain (prevents recursion).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PrimitiveSignature {
    /// Standard secp256k1 ECDSA signature (65 bytes: r, s, v)
    Secp256k1(Signature),
    /// P256 signature with embedded public key (129 bytes)
    P256(P256SignatureWithPreHash),
    /// WebAuthn signature with variable-length authenticator data
    WebAuthn(WebAuthnSignature),
}

impl PrimitiveSignature {
    /// Encode signature to bytes.
    ///
    /// Wire format:
    /// - Secp256k1: 65 bytes (no type prefix, backward compat)
    /// - P256:      0x01 || r(32) || s(32) || pub_key_x(32) || pub_key_y(32) || pre_hash(1) = 130 bytes
    /// - WebAuthn:  0x02 || webauthn_data || r(32) || s(32) || pub_key_x(32) || pub_key_y(32)
    pub fn to_bytes(&self) -> Bytes {
        match self {
            Self::Secp256k1(sig) => {
                let sig_bytes = sig.as_bytes();
                debug_assert_eq!(sig_bytes.len(), SECP256K1_SIGNATURE_LENGTH);
                Bytes::copy_from_slice(&sig_bytes)
            }
            Self::P256(p256_sig) => {
                let mut bytes = Vec::with_capacity(1 + P256_SIGNATURE_LENGTH);
                bytes.push(SIGNATURE_TYPE_P256);
                bytes.extend_from_slice(p256_sig.r.as_slice());
                bytes.extend_from_slice(p256_sig.s.as_slice());
                bytes.extend_from_slice(p256_sig.pub_key_x.as_slice());
                bytes.extend_from_slice(p256_sig.pub_key_y.as_slice());
                bytes.push(if p256_sig.pre_hash { 1 } else { 0 });
                Bytes::from(bytes)
            }
            Self::WebAuthn(webauthn_sig) => {
                let mut bytes = Vec::with_capacity(1 + webauthn_sig.webauthn_data.len() + 128);
                bytes.push(SIGNATURE_TYPE_WEBAUTHN);
                bytes.extend_from_slice(&webauthn_sig.webauthn_data);
                bytes.extend_from_slice(webauthn_sig.r.as_slice());
                bytes.extend_from_slice(webauthn_sig.s.as_slice());
                bytes.extend_from_slice(webauthn_sig.pub_key_x.as_slice());
                bytes.extend_from_slice(webauthn_sig.pub_key_y.as_slice());
                Bytes::from(bytes)
            }
        }
    }
}

impl Encodable for PrimitiveSignature {
    fn encode(&self, out: &mut dyn BufMut) {
        let bytes = self.to_bytes();
        Encodable::encode(&bytes, out);
    }

    fn length(&self) -> usize {
        self.to_bytes().length()
    }
}

// ---------------------------------------------------------------------------
// KeychainVersion
// ---------------------------------------------------------------------------

/// Keychain signature version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeychainVersion {
    /// Legacy (V1): inner signature signs `sig_hash` directly.
    #[default]
    V1,
    /// V2: inner signature signs `keccak256(0x04 || sig_hash || user_address)`.
    V2,
}

// ---------------------------------------------------------------------------
// KeychainSignature
// ---------------------------------------------------------------------------

/// Keychain signature: wraps an inner primitive signature with a user address.
///
/// Wire format (V1): 0x03 || user_address(20) || inner_signature
/// Wire format (V2): 0x04 || user_address(20) || inner_signature
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeychainSignature {
    pub user_address: Address,
    pub signature: PrimitiveSignature,
    #[serde(default)]
    pub version: KeychainVersion,
}

impl PartialEq for KeychainSignature {
    fn eq(&self, other: &Self) -> bool {
        self.user_address == other.user_address
            && self.signature == other.signature
            && self.version == other.version
    }
}

impl Eq for KeychainSignature {}

impl core::hash::Hash for KeychainSignature {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.user_address.hash(state);
        self.signature.hash(state);
        self.version.hash(state);
    }
}

// ---------------------------------------------------------------------------
// TempoSignature
// ---------------------------------------------------------------------------

/// AA transaction signature supporting multiple signature schemes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged, rename_all = "camelCase")]
pub enum TempoSignature {
    /// Primitive: Secp256k1, P256, or WebAuthn.
    Primitive(PrimitiveSignature),
    /// Keychain: wraps another signature with a user address.
    Keychain(KeychainSignature),
}

impl TempoSignature {
    /// Encode signature to bytes.
    ///
    /// Wire format:
    /// - Primitive variants delegate to `PrimitiveSignature::to_bytes()`
    /// - Keychain: type_byte(0x03 or 0x04) || user_address(20) || inner_signature_bytes
    pub fn to_bytes(&self) -> Bytes {
        match self {
            Self::Primitive(primitive_sig) => primitive_sig.to_bytes(),
            Self::Keychain(keychain_sig) => {
                let inner_bytes = keychain_sig.signature.to_bytes();
                let mut bytes = Vec::with_capacity(1 + 20 + inner_bytes.len());
                let type_byte = match keychain_sig.version {
                    KeychainVersion::V1 => SIGNATURE_TYPE_KEYCHAIN,
                    KeychainVersion::V2 => SIGNATURE_TYPE_KEYCHAIN_V2,
                };
                bytes.push(type_byte);
                bytes.extend_from_slice(keychain_sig.user_address.as_slice());
                bytes.extend_from_slice(&inner_bytes);
                Bytes::from(bytes)
            }
        }
    }
}

impl Encodable for TempoSignature {
    fn encode(&self, out: &mut dyn BufMut) {
        let bytes = self.to_bytes();
        Encodable::encode(&bytes, out);
    }

    fn length(&self) -> usize {
        self.to_bytes().length()
    }
}

// ---------------------------------------------------------------------------
// TokenLimit
// ---------------------------------------------------------------------------

/// TIP20 per-token spending limit for access keys.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize,
    alloy::rlp::RlpEncodable,
)]
#[serde(rename_all = "camelCase")]
pub struct TokenLimit {
    pub token: Address,
    pub limit: U256,
}

// ---------------------------------------------------------------------------
// KeyAuthorization
// ---------------------------------------------------------------------------

/// Key authorization for provisioning access keys.
///
/// RLP encoding: `[chain_id, key_type, key_id, expiry?, limits?]`
/// Uses `#[rlp(trailing)]` semantics: optional trailing fields omitted when `None`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyAuthorization {
    #[serde(with = "alloy::serde::quantity")]
    pub chain_id: u64,
    pub key_type: SignatureType,
    pub key_id: Address,
    #[serde(default, with = "alloy::serde::quantity::opt")]
    pub expiry: Option<u64>,
    #[serde(default)]
    pub limits: Option<Vec<TokenLimit>>,
}

/// Manual RLP Encodable to match the writer's `#[rlp(trailing)]` behavior:
/// optional trailing fields are only encoded when present.
impl Encodable for KeyAuthorization {
    fn encode(&self, out: &mut dyn BufMut) {
        let payload = self.fields_len();
        Header { list: true, payload_length: payload }.encode(out);
        self.chain_id.encode(out);
        self.key_type.encode(out);
        self.key_id.encode(out);
        // Trailing optional fields: only encoded if present (or if a later field is present)
        match (&self.expiry, &self.limits) {
            (None, None) => { /* nothing */ }
            (Some(expiry), None) => {
                expiry.encode(out);
            }
            (expiry, Some(limits)) => {
                // If limits is present, expiry must be encoded (even if None -> empty string)
                if let Some(expiry) = expiry {
                    expiry.encode(out);
                } else {
                    out.put_u8(EMPTY_STRING_CODE);
                }
                limits.encode(out);
            }
        }
    }

    fn length(&self) -> usize {
        let payload = self.fields_len();
        payload + length_of_length(payload)
    }
}

impl KeyAuthorization {
    fn fields_len(&self) -> usize {
        let mut len = self.chain_id.length()
            + self.key_type.length()
            + self.key_id.length();
        match (&self.expiry, &self.limits) {
            (None, None) => {}
            (Some(expiry), None) => {
                len += expiry.length();
            }
            (expiry, Some(limits)) => {
                len += expiry.map_or(1, |e| e.length());
                len += limits.length();
            }
        }
        len
    }
}

// ---------------------------------------------------------------------------
// SignedKeyAuthorization
// ---------------------------------------------------------------------------

/// Signed key authorization (key authorization + root key signature).
///
/// RLP: `[chain_id, key_type, key_id, expiry?, limits?, signature?]`
/// The `#[rlp(trailing)]` in the writer means `signature` is trailing after
/// KeyAuthorization's own trailing fields. We match this exactly.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedKeyAuthorization {
    #[serde(flatten)]
    pub authorization: KeyAuthorization,
    pub signature: PrimitiveSignature,
}

impl Encodable for SignedKeyAuthorization {
    fn encode(&self, out: &mut dyn BufMut) {
        let payload = self.fields_len();
        Header { list: true, payload_length: payload }.encode(out);
        // Encode all KeyAuthorization fields inline (not as a nested list)
        self.authorization.chain_id.encode(out);
        self.authorization.key_type.encode(out);
        self.authorization.key_id.encode(out);
        // Trailing: expiry, limits, signature
        // Since signature is always present, we must encode expiry and limits too
        // (even if None -> empty string) to maintain positional correctness.
        if let Some(expiry) = self.authorization.expiry {
            expiry.encode(out);
        } else {
            out.put_u8(EMPTY_STRING_CODE);
        }
        if let Some(ref limits) = self.authorization.limits {
            limits.encode(out);
        } else {
            out.put_u8(EMPTY_STRING_CODE);
        }
        self.signature.encode(out);
    }

    fn length(&self) -> usize {
        let payload = self.fields_len();
        payload + length_of_length(payload)
    }
}

impl SignedKeyAuthorization {
    fn fields_len(&self) -> usize {
        self.authorization.chain_id.length()
            + self.authorization.key_type.length()
            + self.authorization.key_id.length()
            + self.authorization.expiry.map_or(1, |e| e.length())
            + self.authorization.limits.as_ref().map_or(1, |l| l.length())
            + self.signature.length()
    }
}

// ---------------------------------------------------------------------------
// TempoSignedAuthorization (EIP-7702 with TempoSignature)
// ---------------------------------------------------------------------------

/// A signed EIP-7702 authorization using `TempoSignature`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TempoSignedAuthorization {
    #[serde(flatten)]
    pub inner: Authorization,
    pub signature: TempoSignature,
}

impl Encodable for TempoSignedAuthorization {
    fn encode(&self, buf: &mut dyn BufMut) {
        let payload = self.fields_len();
        Header { list: true, payload_length: payload }.encode(buf);
        self.inner.chain_id.encode(buf);
        self.inner.address.encode(buf);
        self.inner.nonce.encode(buf);
        self.signature.encode(buf);
    }

    fn length(&self) -> usize {
        let len = self.fields_len();
        len + length_of_length(len)
    }
}

impl TempoSignedAuthorization {
    fn fields_len(&self) -> usize {
        self.inner.chain_id.length()
            + self.inner.address.length()
            + self.inner.nonce.length()
            + self.signature.length()
    }
}

// ---------------------------------------------------------------------------
// Call
// ---------------------------------------------------------------------------

/// A single call within a Tempo batch transaction.
///
/// RLP encoding: `[to, value, input]`
/// - `to`: `TxKind::Create` encodes as empty bytes (0x80), `TxKind::Call(addr)` as 20-byte address
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Call {
    /// Call target. `None` / create encodes as empty string in RLP.
    pub to: Option<Address>,
    /// Call value.
    pub value: U256,
    /// Call input data.
    #[serde(flatten, with = "serde_input")]
    pub input: Bytes,
}

impl Encodable for Call {
    fn encode(&self, out: &mut dyn BufMut) {
        let payload = self.payload_len();
        Header { list: true, payload_length: payload }.encode(out);
        // `to` field: None (create) -> empty string (0x80), Some(addr) -> 20-byte address
        match self.to {
            Some(addr) => addr.encode(out),
            None => out.put_u8(EMPTY_STRING_CODE),
        }
        self.value.encode(out);
        self.input.encode(out);
    }

    fn length(&self) -> usize {
        let payload = self.payload_len();
        payload + length_of_length(payload)
    }
}

impl Call {
    fn payload_len(&self) -> usize {
        let to_len = match self.to {
            Some(ref addr) => addr.length(),
            None => 1, // EMPTY_STRING_CODE
        };
        to_len + self.value.length() + self.input.length()
    }
}

// ---------------------------------------------------------------------------
// serde_input helper (flattened input/data field for Call)
// ---------------------------------------------------------------------------

mod serde_input {
    use super::*;
    use serde::{Deserializer, Serializer};
    use std::borrow::Cow;

    #[derive(Serialize, Deserialize)]
    struct SerdeHelper<'a> {
        input: Option<Cow<'a, Bytes>>,
        data: Option<Cow<'a, Bytes>>,
    }

    pub(super) fn serialize<S>(input: &Bytes, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        SerdeHelper {
            input: Some(Cow::Borrowed(input)),
            data: None,
        }
        .serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        let helper = SerdeHelper::deserialize(deserializer)?;
        Ok(helper
            .input
            .or(helper.data)
            .ok_or(serde::de::Error::missing_field(
                "missing `input` or `data` field",
            ))?
            .into_owned())
    }
}

// ---------------------------------------------------------------------------
// fee_payer_signature_hash + recover_fee_payer
// ---------------------------------------------------------------------------

/// Helper: create an RLP list header.
#[inline]
fn rlp_header(payload_length: usize) -> Header {
    Header {
        list: true,
        payload_length,
    }
}

/// Compute the RLP payload length for the fee payer signature hash.
///
/// Field order (matching writer's `rlp_encoded_fields_length`):
///   chain_id, max_priority_fee_per_gas, max_fee_per_gas, gas_limit,
///   calls, access_list, nonce_key, nonce,
///   valid_before, valid_after, fee_token,
///   sender,                           // <-- fee_payer slot = sender address
///   tempo_authorization_list,
///   [key_authorization]               // <-- only if present
#[allow(clippy::too_many_arguments)]
fn fee_payer_payload_length(
    chain_id: u64,
    max_priority_fee_per_gas: u128,
    max_fee_per_gas: u128,
    gas_limit: u64,
    calls: &[Call],
    access_list: &AccessList,
    nonce_key: U256,
    nonce: u64,
    valid_before: Option<u64>,
    valid_after: Option<u64>,
    fee_token: Option<Address>,
    sender: Address,
    tempo_authorization_list: &[TempoSignedAuthorization],
    key_authorization: Option<&SignedKeyAuthorization>,
) -> usize {
    chain_id.length()
        + max_priority_fee_per_gas.length()
        + max_fee_per_gas.length()
        + gas_limit.length()
        + list_length(calls)
        + access_list.length()
        + nonce_key.length()
        + nonce.length()
        + valid_before.map_or(1, |v| v.length())   // None -> EMPTY_STRING_CODE (1 byte)
        + valid_after.map_or(1, |v| v.length())
        + fee_token.map_or(1, |a| a.length())       // fee_token IS included (skip_fee_token=false)
        + sender.length()                            // fee_payer slot = sender address
        + list_length(tempo_authorization_list)
        + key_authorization.map_or(0, |k| k.length()) // truly optional: 0 bytes when None
}

/// Encode the RLP fields for the fee payer signature hash.
#[allow(clippy::too_many_arguments)]
fn fee_payer_encode_fields(
    out: &mut dyn BufMut,
    chain_id: u64,
    max_priority_fee_per_gas: u128,
    max_fee_per_gas: u128,
    gas_limit: u64,
    calls: &[Call],
    access_list: &AccessList,
    nonce_key: U256,
    nonce: u64,
    valid_before: Option<u64>,
    valid_after: Option<u64>,
    fee_token: Option<Address>,
    sender: Address,
    tempo_authorization_list: &[TempoSignedAuthorization],
    key_authorization: Option<&SignedKeyAuthorization>,
) {
    chain_id.encode(out);
    max_priority_fee_per_gas.encode(out);
    max_fee_per_gas.encode(out);
    gas_limit.encode(out);
    encode_list(calls, out);
    access_list.encode(out);
    nonce_key.encode(out);
    nonce.encode(out);

    // valid_before (optional u64)
    if let Some(valid_before) = valid_before {
        valid_before.encode(out);
    } else {
        out.put_u8(EMPTY_STRING_CODE);
    }

    // valid_after (optional u64)
    if let Some(valid_after) = valid_after {
        valid_after.encode(out);
    } else {
        out.put_u8(EMPTY_STRING_CODE);
    }

    // fee_token (optional Address) - included for fee payer (skip_fee_token = false)
    if let Some(addr) = fee_token {
        addr.encode(out);
    } else {
        out.put_u8(EMPTY_STRING_CODE);
    }

    // fee_payer slot: encode sender address
    sender.encode(out);

    // authorization_list
    encode_list(tempo_authorization_list, out);

    // key_authorization: only encoded if present (truly optional trailing field)
    if let Some(key_auth) = key_authorization {
        key_auth.encode(out);
    }
}

/// Compute the fee payer signature hash for a Tempo transaction.
///
/// This hash is what the fee payer (sponsor) signs to authorize gas sponsorship.
/// The encoding is: `keccak256(0x78 || rlp([fields...]))` where the fee_payer_signature
/// slot is replaced with the sender address.
#[allow(clippy::too_many_arguments)]
pub fn fee_payer_signature_hash(
    chain_id: u64,
    max_priority_fee_per_gas: u128,
    max_fee_per_gas: u128,
    gas_limit: u64,
    calls: &[Call],
    access_list: &AccessList,
    nonce_key: U256,
    nonce: u64,
    valid_before: Option<u64>,
    valid_after: Option<u64>,
    fee_token: Option<Address>,
    sender: Address,
    tempo_authorization_list: &[TempoSignedAuthorization],
    key_authorization: Option<&SignedKeyAuthorization>,
) -> B256 {
    let payload_length = fee_payer_payload_length(
        chain_id,
        max_priority_fee_per_gas,
        max_fee_per_gas,
        gas_limit,
        calls,
        access_list,
        nonce_key,
        nonce,
        valid_before,
        valid_after,
        fee_token,
        sender,
        tempo_authorization_list,
        key_authorization,
    );

    let mut buf = Vec::with_capacity(1 + rlp_header(payload_length).length_with_payload());

    // Magic byte
    buf.put_u8(FEE_PAYER_SIGNATURE_MAGIC_BYTE);

    // RLP header
    rlp_header(payload_length).encode(&mut buf);

    // Fields
    fee_payer_encode_fields(
        &mut buf,
        chain_id,
        max_priority_fee_per_gas,
        max_fee_per_gas,
        gas_limit,
        calls,
        access_list,
        nonce_key,
        nonce,
        valid_before,
        valid_after,
        fee_token,
        sender,
        tempo_authorization_list,
        key_authorization,
    );

    keccak256(&buf)
}

/// Recover the fee payer (sponsor) address from a fee payer signature.
///
/// Returns `Some(address)` on successful recovery, `None` on failure.
#[allow(clippy::too_many_arguments)]
pub fn recover_fee_payer(
    fee_payer_signature: &Signature,
    chain_id: u64,
    max_priority_fee_per_gas: u128,
    max_fee_per_gas: u128,
    gas_limit: u64,
    calls: &[Call],
    access_list: &AccessList,
    nonce_key: U256,
    nonce: u64,
    valid_before: Option<u64>,
    valid_after: Option<u64>,
    fee_token: Option<Address>,
    sender: Address,
    tempo_authorization_list: &[TempoSignedAuthorization],
    key_authorization: Option<&SignedKeyAuthorization>,
) -> Option<Address> {
    let hash = fee_payer_signature_hash(
        chain_id,
        max_priority_fee_per_gas,
        max_fee_per_gas,
        gas_limit,
        calls,
        access_list,
        nonce_key,
        nonce,
        valid_before,
        valid_after,
        fee_token,
        sender,
        tempo_authorization_list,
        key_authorization,
    );

    fee_payer_signature
        .recover_address_from_prehash(&hash)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_call_rlp_encoding() {
        // Call with an address target
        let call = Call {
            to: Some(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let mut buf = Vec::new();
        call.encode(&mut buf);
        assert_eq!(buf.len(), call.length());

        // CREATE call (to = None)
        let create_call = Call {
            to: None,
            value: U256::from(1u64),
            input: Bytes::from(vec![0xab, 0xcd]),
        };
        let mut buf2 = Vec::new();
        create_call.encode(&mut buf2);
        assert_eq!(buf2.len(), create_call.length());
    }

    #[test]
    fn test_tempo_signed_authorization_rlp() {
        let auth = TempoSignedAuthorization {
            inner: Authorization {
                chain_id: U256::from(1u64),
                address: Address::ZERO,
                nonce: 1,
            },
            signature: TempoSignature::Primitive(PrimitiveSignature::Secp256k1(
                Signature::test_signature(),
            )),
        };
        let mut buf = Vec::new();
        auth.encode(&mut buf);
        assert_eq!(buf.len(), auth.length());
        assert!(!buf.is_empty());
    }

    #[test]
    fn test_fee_payer_signature_hash_deterministic() {
        let calls = vec![Call {
            to: Some(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        }];
        let access_list = AccessList::default();
        let sender = Address::ZERO;

        let hash1 = fee_payer_signature_hash(
            1, 0, 0, 21000, &calls, &access_list,
            U256::ZERO, 0, None, None, None, sender, &[], None,
        );
        let hash2 = fee_payer_signature_hash(
            1, 0, 0, 21000, &calls, &access_list,
            U256::ZERO, 0, None, None, None, sender, &[], None,
        );
        assert_eq!(hash1, hash2, "fee_payer_signature_hash must be deterministic");
        assert_ne!(hash1, B256::ZERO);
    }

    #[test]
    fn test_fee_payer_different_params_different_hash() {
        let calls = vec![Call {
            to: Some(Address::ZERO),
            value: U256::ZERO,
            input: Bytes::new(),
        }];
        let access_list = AccessList::default();
        let sender = Address::ZERO;

        let hash_a = fee_payer_signature_hash(
            1, 0, 0, 21000, &calls, &access_list,
            U256::ZERO, 0, None, None, None, sender, &[], None,
        );
        // Different chain_id
        let hash_b = fee_payer_signature_hash(
            2, 0, 0, 21000, &calls, &access_list,
            U256::ZERO, 0, None, None, None, sender, &[], None,
        );
        assert_ne!(hash_a, hash_b);

        // Different sender
        let hash_c = fee_payer_signature_hash(
            1, 0, 0, 21000, &calls, &access_list,
            U256::ZERO, 0, None, None, None,
            Address::with_last_byte(1), &[], None,
        );
        assert_ne!(hash_a, hash_c);
    }

    #[test]
    fn test_signed_key_authorization_rlp() {
        let signed = SignedKeyAuthorization {
            authorization: KeyAuthorization {
                chain_id: 1,
                key_type: SignatureType::Secp256k1,
                key_id: Address::ZERO,
                expiry: Some(1000),
                limits: None,
            },
            signature: PrimitiveSignature::Secp256k1(Signature::test_signature()),
        };
        let mut buf = Vec::new();
        signed.encode(&mut buf);
        assert_eq!(buf.len(), signed.length());
    }

    #[test]
    fn test_key_authorization_rlp_trailing() {
        // No optional fields
        let auth1 = KeyAuthorization {
            chain_id: 1,
            key_type: SignatureType::Secp256k1,
            key_id: Address::ZERO,
            expiry: None,
            limits: None,
        };
        let mut buf1 = Vec::new();
        auth1.encode(&mut buf1);

        // With expiry only
        let auth2 = KeyAuthorization {
            chain_id: 1,
            key_type: SignatureType::Secp256k1,
            key_id: Address::ZERO,
            expiry: Some(1000),
            limits: None,
        };
        let mut buf2 = Vec::new();
        auth2.encode(&mut buf2);

        // With expiry and limits
        let auth3 = KeyAuthorization {
            chain_id: 1,
            key_type: SignatureType::Secp256k1,
            key_id: Address::ZERO,
            expiry: Some(1000),
            limits: Some(vec![TokenLimit {
                token: Address::ZERO,
                limit: U256::from(100u64),
            }]),
        };
        let mut buf3 = Vec::new();
        auth3.encode(&mut buf3);

        // Each should have a valid length
        assert_eq!(buf1.len(), auth1.length());
        assert_eq!(buf2.len(), auth2.length());
        assert_eq!(buf3.len(), auth3.length());

        // Trailing fields make it longer
        assert!(buf2.len() > buf1.len());
        assert!(buf3.len() > buf2.len());
    }
}
