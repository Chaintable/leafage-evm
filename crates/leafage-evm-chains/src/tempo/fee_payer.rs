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
use alloy::primitives::{Address, Bytes, Signature, B256, U256, keccak256, uint};
use alloy_rlp::{BufMut, Encodable, Header, EMPTY_STRING_CODE, encode_list, length_of_length, list_length};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Magic byte prefix for the fee payer signature hash.
pub const FEE_PAYER_SIGNATURE_MAGIC_BYTE: u8 = 0x78;

/// Secp256k1 signature wire length (no type prefix).
const SECP256K1_SIGNATURE_LENGTH: usize = 65;

/// P256 signature wire length (excluding the 1-byte type prefix).
const P256_SIGNATURE_LENGTH: usize = 129;

/// Max WebAuthn signature length (2 KB ceiling on `webauthn_data` + trailing 128 bytes of
/// r/s/x/y; see Tempo writer `tempo_transaction.rs`).
const MAX_WEBAUTHN_SIGNATURE_LENGTH: usize = 2048;

/// Signature type prefix bytes.
const SIGNATURE_TYPE_P256: u8 = 0x01;
const SIGNATURE_TYPE_WEBAUTHN: u8 = 0x02;
const SIGNATURE_TYPE_KEYCHAIN: u8 = 0x03;
const SIGNATURE_TYPE_KEYCHAIN_V2: u8 = 0x04;

/// Half of the P256 curve order (n/2). ECDSA signatures require `s <= n/2` (low-s) to
/// prevent malleability.
const P256N_HALF: U256 =
    uint!(0x7FFFFFFF800000007FFFFFFFFFFFFFFFDE737D56D38BCF4279DCE5617E3192A8_U256);

/// Minimum WebAuthn authenticatorData length: 32 rpIdHash + 1 flags + 4 signCount.
const MIN_AUTH_DATA_LEN: usize = 37;

// WebAuthn authenticatorData flags (byte 32).
// ref: https://www.w3.org/TR/webauthn-2/#sctn-authenticator-data
const WA_UP: u8 = 0x01; // User Presence (bit 0)
const WA_UV: u8 = 0x04; // User Verified (bit 2)
const WA_AT: u8 = 0x40; // Attested credential data (bit 6)
const WA_ED: u8 = 0x80; // Extension data present (bit 7)

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

    /// Parse a signature from wire bytes (inverse of `to_bytes`).
    ///
    /// Wire format:
    /// - 65 bytes (no type prefix) -> Secp256k1 (backward compat)
    /// - `0x01 || r(32) || s(32) || x(32) || y(32) || pre_hash(1)` -> P256
    /// - `0x02 || webauthn_data || r(32) || s(32) || x(32) || y(32)` -> WebAuthn
    ///
    /// Ported from Tempo writer `tt_signature.rs::PrimitiveSignature::from_bytes`.
    pub fn from_bytes(data: &[u8]) -> Result<Self, &'static str> {
        if data.is_empty() {
            return Err("Signature data is empty");
        }

        // Backward compat: exactly 65 bytes => secp256k1 without type identifier.
        if data.len() == SECP256K1_SIGNATURE_LENGTH {
            let sig = Signature::try_from(data)
                .map_err(|_| "Failed to parse secp256k1 signature: invalid signature values")?;
            return Ok(Self::Secp256k1(sig));
        }

        if data.len() < 2 {
            return Err("Signature data too short: expected type identifier + signature data");
        }

        let type_id = data[0];
        let sig_data = &data[1..];

        match type_id {
            SIGNATURE_TYPE_P256 => {
                if sig_data.len() != P256_SIGNATURE_LENGTH {
                    return Err("Invalid P256 signature length");
                }
                Ok(Self::P256(P256SignatureWithPreHash {
                    r: B256::from_slice(&sig_data[0..32]),
                    s: B256::from_slice(&sig_data[32..64]),
                    pub_key_x: B256::from_slice(&sig_data[64..96]),
                    pub_key_y: B256::from_slice(&sig_data[96..128]),
                    pre_hash: sig_data[128] != 0,
                }))
            }
            SIGNATURE_TYPE_WEBAUTHN => {
                let len = sig_data.len();
                if !(128..=MAX_WEBAUTHN_SIGNATURE_LENGTH).contains(&len) {
                    return Err("Invalid WebAuthn signature length");
                }
                Ok(Self::WebAuthn(WebAuthnSignature {
                    r: B256::from_slice(&sig_data[len - 128..len - 96]),
                    s: B256::from_slice(&sig_data[len - 96..len - 64]),
                    pub_key_x: B256::from_slice(&sig_data[len - 64..len - 32]),
                    pub_key_y: B256::from_slice(&sig_data[len - 32..]),
                    webauthn_data: Bytes::copy_from_slice(&sig_data[..len - 128]),
                }))
            }
            _ => Err("Unknown signature type identifier"),
        }
    }

    /// Recover the signer address from this signature over `sig_hash`.
    ///
    /// - Secp256k1: standard ecrecover (alloy `Signature::recover_address_from_prehash`).
    /// - P256: verifies the signature (with low-s malleability check) and derives
    ///   the address from the embedded public key.
    /// - WebAuthn: parses authenticatorData + clientDataJSON, validates the challenge
    ///   matches `sig_hash`, then verifies the P256 signature over
    ///   `sha256(authenticatorData || sha256(clientDataJSON))`.
    ///
    /// Ported from Tempo writer `tt_signature.rs::PrimitiveSignature::recover_signer`.
    pub fn recover_signer(&self, sig_hash: &B256) -> Result<Address, &'static str> {
        match self {
            Self::Secp256k1(sig) => sig
                .recover_address_from_prehash(sig_hash)
                .map_err(|_| "secp256k1 recovery failed"),
            Self::P256(p256_sig) => {
                let message_hash = if p256_sig.pre_hash {
                    // Some P256 implementations (e.g. Web Crypto) pre-hash the digest.
                    B256::from_slice(Sha256::digest(sig_hash.as_slice()).as_ref())
                } else {
                    *sig_hash
                };

                verify_p256_signature_internal(
                    p256_sig.r.as_slice(),
                    p256_sig.s.as_slice(),
                    p256_sig.pub_key_x.as_slice(),
                    p256_sig.pub_key_y.as_slice(),
                    &message_hash,
                )?;

                Ok(derive_p256_address(
                    &p256_sig.pub_key_x,
                    &p256_sig.pub_key_y,
                ))
            }
            Self::WebAuthn(webauthn_sig) => {
                let message_hash =
                    verify_webauthn_data_internal(&webauthn_sig.webauthn_data, sig_hash)?;

                verify_p256_signature_internal(
                    webauthn_sig.r.as_slice(),
                    webauthn_sig.s.as_slice(),
                    webauthn_sig.pub_key_x.as_slice(),
                    webauthn_sig.pub_key_y.as_slice(),
                    &message_hash,
                )?;

                Ok(derive_p256_address(
                    &webauthn_sig.pub_key_x,
                    &webauthn_sig.pub_key_y,
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Signature verification helpers (P256 + WebAuthn)
// ---------------------------------------------------------------------------

/// Derive a P256 address from the public key coordinates.
///
/// `address = keccak256(x || y)[12..]` (last 20 bytes), matching Tempo writer.
fn derive_p256_address(pub_key_x: &B256, pub_key_y: &B256) -> Address {
    let hash = keccak256([pub_key_x.as_slice(), pub_key_y.as_slice()].concat());
    Address::from_slice(&hash[12..])
}

/// Verify a P256 ECDSA signature with low-s malleability guard.
///
/// `message_hash` is the already-hashed 32-byte digest (no further hashing inside).
fn verify_p256_signature_internal(
    r: &[u8],
    s: &[u8],
    pub_key_x: &[u8],
    pub_key_y: &[u8],
    message_hash: &B256,
) -> Result<(), &'static str> {
    // Low-s check (reject s > n/2 to prevent malleability).
    if U256::from_be_slice(s) > P256N_HALF {
        return Err("P256 signature has high s value");
    }

    use p256::{
        ecdsa::{signature::hazmat::PrehashVerifier, Signature as P256Signature, VerifyingKey},
        EncodedPoint,
    };

    let encoded_point =
        EncodedPoint::from_affine_coordinates(pub_key_x.into(), pub_key_y.into(), false);
    let verifying_key =
        VerifyingKey::from_encoded_point(&encoded_point).map_err(|_| "Invalid P256 public key")?;

    let mut sig_bytes = [0u8; 64];
    sig_bytes[..32].copy_from_slice(r);
    sig_bytes[32..].copy_from_slice(s);
    let signature = P256Signature::from_slice(&sig_bytes)
        .map_err(|_| "Invalid P256 signature encoding")?;

    verifying_key
        .verify_prehash(message_hash.as_slice(), &signature)
        .map_err(|_| "P256 signature verification failed")
}

/// Minimal `clientDataJSON` shape — only the fields we validate.
/// `serde_json` ignores unknown fields, so additional keys (origin, crossOrigin, …) are tolerated.
#[derive(Deserialize)]
struct WebAuthnClientDataJson<'a> {
    #[serde(rename = "type")]
    type_field: &'a str,
    challenge: &'a str,
}

/// Parse + validate WebAuthn `authenticatorData || clientDataJSON`, returning the
/// message hash that the P256 signature signs:
/// `messageHash = sha256(authenticatorData || sha256(clientDataJSON))`.
fn verify_webauthn_data_internal(
    webauthn_data: &[u8],
    tx_hash: &B256,
) -> Result<B256, &'static str> {
    if webauthn_data.len() < MIN_AUTH_DATA_LEN + 32 {
        return Err("WebAuthn data too short");
    }

    let flags = webauthn_data[32];
    let up_flag = flags & WA_UP;
    let uv_flag = flags & WA_UV;
    let at_flag = flags & WA_AT;
    let ed_flag = flags & WA_ED;

    // UP or UV MUST be set (UV implies user presence per WebAuthn spec).
    if up_flag == 0 && uv_flag == 0 {
        return Err("neither UP, nor UV flag set");
    }
    // AT must NOT be set for assertion signatures (webauthn.get).
    if at_flag != 0 {
        return Err("AT flag must not be set for assertion signatures");
    }
    // ED must NOT be set — Tempo AA does not support extensions (would require CBOR).
    if ed_flag != 0 {
        return Err("ED flag must not be set, as Tempo doesn't support extensions");
    }

    let auth_data_len = MIN_AUTH_DATA_LEN;
    let authenticator_data = &webauthn_data[..auth_data_len];
    let client_data_json = &webauthn_data[auth_data_len..];

    let client_data: WebAuthnClientDataJson<'_> = serde_json::from_slice(client_data_json)
        .map_err(|_| "clientDataJSON is not valid JSON")?;

    if client_data.type_field != "webauthn.get" {
        return Err("clientDataJSON type must be webauthn.get");
    }

    if client_data.challenge != URL_SAFE_NO_PAD.encode(tx_hash.as_slice()) {
        return Err("clientDataJSON challenge does not match transaction hash");
    }

    let client_data_hash = Sha256::digest(client_data_json);
    let mut hasher = Sha256::new();
    hasher.update(authenticator_data);
    hasher.update(client_data_hash);
    Ok(B256::from_slice(&hasher.finalize()))
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

// `CallScope` / `SelectorRule` live in `leafage-evm-types` (re-exported via
// the `rpc::call` module) so the RPC layer can deserialize them directly
// from JSON. We re-import them here for use inside `KeyAuthorization`.
pub use leafage_evm_types::{CallScope, SelectorRule};

/// Key authorization for provisioning access keys.
///
/// RLP encoding: `[chain_id, key_type, key_id, expiry?, limits?, allowed_calls?]`
/// Uses `#[rlp(trailing(canonical))]` semantics: trailing optionals are omitted
/// when `None` (canonical) and any `None` preceding a `Some` is encoded as the
/// empty bytestring `0x80` for positional correctness.
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
    /// TIP-1011 (T3+) per-target call scopes. `None` = unrestricted; `Some([])`
    /// = scoped deny-all; `Some([scope, ...])` = the listed scopes.
    #[serde(default)]
    pub allowed_calls: Option<Vec<CallScope>>,
}

/// Manual RLP Encodable to match the writer's `#[rlp(trailing(canonical))]`
/// behavior: trailing optionals are omitted when `None`, but a `None`
/// preceding any later `Some` is encoded positionally as the empty bytestring
/// `0x80`.
impl Encodable for KeyAuthorization {
    fn encode(&self, out: &mut dyn BufMut) {
        let payload = self.fields_len();
        Header { list: true, payload_length: payload }.encode(out);
        self.chain_id.encode(out);
        self.key_type.encode(out);
        self.key_id.encode(out);

        let last_present = self.last_trailing_present();
        if last_present >= 1 {
            match &self.expiry {
                Some(expiry) => expiry.encode(out),
                None => out.put_u8(EMPTY_STRING_CODE),
            }
        }
        if last_present >= 2 {
            match &self.limits {
                Some(limits) => limits.encode(out),
                None => out.put_u8(EMPTY_STRING_CODE),
            }
        }
        if last_present >= 3 {
            match &self.allowed_calls {
                Some(scopes) => scopes.encode(out),
                None => out.put_u8(EMPTY_STRING_CODE),
            }
        }
    }

    fn length(&self) -> usize {
        let payload = self.fields_len();
        payload + length_of_length(payload)
    }
}

impl KeyAuthorization {
    /// Returns the 1-indexed position of the latest `Some` trailing field
    /// (1=expiry, 2=limits, 3=allowed_calls), or 0 if all are `None`. Used to
    /// decide which preceding `None`s require positional 0x80 encoding.
    fn last_trailing_present(&self) -> u8 {
        if self.allowed_calls.is_some() {
            3
        } else if self.limits.is_some() {
            2
        } else if self.expiry.is_some() {
            1
        } else {
            0
        }
    }

    fn fields_len(&self) -> usize {
        let mut len = self.chain_id.length()
            + self.key_type.length()
            + self.key_id.length();
        let last_present = self.last_trailing_present();
        if last_present >= 1 {
            len += self.expiry.map_or(1, |e| e.length());
        }
        if last_present >= 2 {
            len += self.limits.as_ref().map_or(1, |l| l.length());
        }
        if last_present >= 3 {
            len += self.allowed_calls.as_ref().map_or(1, |s| s.length());
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
        // Encode all KeyAuthorization fields inline (not as a nested list).
        self.authorization.chain_id.encode(out);
        self.authorization.key_type.encode(out);
        self.authorization.key_id.encode(out);
        // signature is always present at the trailing position, so every
        // preceding optional must be encoded positionally (None -> 0x80).
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
        if let Some(ref scopes) = self.authorization.allowed_calls {
            scopes.encode(out);
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
            + self.authorization.allowed_calls.as_ref().map_or(1, |s| s.length())
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
                allowed_calls: None,
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
            allowed_calls: None,
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
            allowed_calls: None,
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
            allowed_calls: None,
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

    #[test]
    fn key_authorization_with_allowed_calls_grows_encoding() {
        use alloy::primitives::address;

        let scope = CallScope {
            target: address!("0x20C0000000000000000000000000000000000042"),
            selector_rules: vec![SelectorRule {
                selector: alloy::primitives::FixedBytes::from([0xa9, 0x05, 0x9c, 0xbb]),
                recipients: vec![address!("0x1111111111111111111111111111111111111111")],
            }],
        };

        let base = KeyAuthorization {
            chain_id: 1,
            key_type: SignatureType::Secp256k1,
            key_id: Address::ZERO,
            expiry: Some(1000),
            limits: None,
            allowed_calls: None,
        };
        let mut base_buf = Vec::new();
        base.encode(&mut base_buf);

        let with_scopes = KeyAuthorization {
            allowed_calls: Some(vec![scope.clone()]),
            ..base.clone()
        };
        let mut scopes_buf = Vec::new();
        with_scopes.encode(&mut scopes_buf);

        assert_eq!(scopes_buf.len(), with_scopes.length());
        // Adding allowed_calls forces both `limits` (None -> 0x80) and the new
        // field to be encoded positionally, so the buffer must grow.
        assert!(
            scopes_buf.len() > base_buf.len(),
            "with_scopes ({}) should be longer than base ({})",
            scopes_buf.len(),
            base_buf.len(),
        );
    }

    #[test]
    fn key_authorization_deny_all_allowed_calls_round_trips_in_length() {
        // `Some(vec![])` encodes as the empty list `0xc0`, not as `None`.
        let deny_all = KeyAuthorization {
            chain_id: 1,
            key_type: SignatureType::Secp256k1,
            key_id: Address::ZERO,
            expiry: Some(1000),
            limits: None,
            allowed_calls: Some(Vec::new()),
        };
        let mut buf = Vec::new();
        deny_all.encode(&mut buf);
        assert_eq!(buf.len(), deny_all.length());
        // Sanity: contains an explicit empty-list byte after the limits 0x80.
        assert!(
            buf.contains(&0xc0),
            "deny-all allowed_calls should be encoded as empty list 0xc0",
        );
    }

    #[test]
    fn signed_key_authorization_includes_allowed_calls_positionally() {
        // signature is always trailing → both `limits` (None) and
        // `allowed_calls` (Some(vec![])) must take slots before signature.
        let signed = SignedKeyAuthorization {
            authorization: KeyAuthorization {
                chain_id: 1,
                key_type: SignatureType::Secp256k1,
                key_id: Address::ZERO,
                expiry: Some(1000),
                limits: None,
                allowed_calls: Some(Vec::new()),
            },
            signature: PrimitiveSignature::Secp256k1(Signature::test_signature()),
        };
        let mut buf = Vec::new();
        signed.encode(&mut buf);
        assert_eq!(buf.len(), signed.length());
    }

    // ========================================================================
    // from_bytes / recover_signer (T3 signature_verifier prerequisites)
    // ========================================================================

    #[test]
    fn from_bytes_empty_rejected() {
        assert!(PrimitiveSignature::from_bytes(&[]).is_err());
    }

    #[test]
    fn from_bytes_secp256k1_round_trip() {
        let sig = Signature::test_signature();
        let bytes = PrimitiveSignature::Secp256k1(sig).to_bytes();
        assert_eq!(bytes.len(), 65);
        let parsed = PrimitiveSignature::from_bytes(&bytes).expect("65-byte secp256k1 parses");
        assert!(matches!(parsed, PrimitiveSignature::Secp256k1(_)));
    }

    #[test]
    fn from_bytes_unknown_type_rejected() {
        let mut data = vec![0x05u8]; // unknown type byte
        data.extend_from_slice(&[0u8; 129]);
        assert!(PrimitiveSignature::from_bytes(&data).is_err());
    }

    #[test]
    fn from_bytes_p256_wrong_length_rejected() {
        let mut data = vec![SIGNATURE_TYPE_P256];
        data.extend_from_slice(&[0u8; 128]); // 128 not P256_SIGNATURE_LENGTH (129)
        assert!(PrimitiveSignature::from_bytes(&data).is_err());
    }

    #[test]
    fn from_bytes_webauthn_too_short_rejected() {
        let mut data = vec![SIGNATURE_TYPE_WEBAUTHN];
        data.extend_from_slice(&[0u8; 127]); // min payload is 128
        assert!(PrimitiveSignature::from_bytes(&data).is_err());
    }

    #[test]
    fn from_bytes_webauthn_too_long_rejected() {
        let mut data = vec![SIGNATURE_TYPE_WEBAUTHN];
        data.extend_from_slice(&[0u8; MAX_WEBAUTHN_SIGNATURE_LENGTH + 1]);
        assert!(PrimitiveSignature::from_bytes(&data).is_err());
    }

    /// Build a `(PrimitiveSignature::P256, expected_address, message_hash)` triple by
    /// generating a fresh P256 keypair, signing a fixed message, and normalising s.
    fn make_p256_test_sig(msg_hash: B256) -> (PrimitiveSignature, Address) {
        use p256::ecdsa::{signature::hazmat::PrehashSigner, Signature as P256Signature, SigningKey};
        use p256::elliptic_curve::rand_core::OsRng;

        let signing_key = SigningKey::random(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let encoded = verifying_key.to_encoded_point(false);
        let pub_key_x = B256::from_slice(encoded.x().expect("p256 encoded point has x"));
        let pub_key_y = B256::from_slice(encoded.y().expect("p256 encoded point has y"));

        let sig: P256Signature = signing_key
            .sign_prehash(msg_hash.as_slice())
            .expect("p256 prehash sign");
        let normalized = sig.normalize_s().unwrap_or(sig);
        let r = B256::from_slice(&normalized.r().to_bytes());
        let s = B256::from_slice(&normalized.s().to_bytes());

        let prim = PrimitiveSignature::P256(P256SignatureWithPreHash {
            r,
            s,
            pub_key_x,
            pub_key_y,
            pre_hash: false,
        });
        let addr = derive_p256_address(&pub_key_x, &pub_key_y);
        (prim, addr)
    }

    #[test]
    fn p256_recover_round_trip() {
        let msg = B256::from([0xBB; 32]);
        let (prim, expected) = make_p256_test_sig(msg);
        let recovered = prim.recover_signer(&msg).expect("P256 recover");
        assert_eq!(recovered, expected);

        // Round-trip the wire encoding.
        let wire = prim.to_bytes();
        let parsed = PrimitiveSignature::from_bytes(&wire).expect("P256 parses");
        let recovered2 = parsed.recover_signer(&msg).expect("P256 recover after round-trip");
        assert_eq!(recovered2, expected);
    }

    #[test]
    fn p256_recover_with_wrong_hash_fails() {
        let msg = B256::from([0xBB; 32]);
        let (prim, _expected) = make_p256_test_sig(msg);
        let wrong_msg = B256::from([0xCC; 32]);
        assert!(prim.recover_signer(&wrong_msg).is_err());
    }

    #[test]
    fn p256_high_s_rejected() {
        // s = P256N_HALF + 1 is the smallest high-s value.
        let high_s_u256 = P256N_HALF.saturating_add(U256::from(1u64));
        let prim = PrimitiveSignature::P256(P256SignatureWithPreHash {
            r: B256::ZERO,
            s: B256::from(high_s_u256.to_be_bytes::<32>()),
            pub_key_x: B256::ZERO,
            pub_key_y: B256::ZERO,
            pre_hash: false,
        });
        assert!(prim.recover_signer(&B256::ZERO).is_err());
    }

    #[test]
    fn webauthn_too_short_data_rejected() {
        // Less than MIN_AUTH_DATA_LEN + 32 (need at least 37 + 32 = 69 bytes).
        let sig = PrimitiveSignature::WebAuthn(WebAuthnSignature {
            r: B256::ZERO,
            s: B256::ZERO,
            pub_key_x: B256::ZERO,
            pub_key_y: B256::ZERO,
            webauthn_data: Bytes::from(vec![0u8; MIN_AUTH_DATA_LEN + 31]),
        });
        assert!(sig.recover_signer(&B256::ZERO).is_err());
    }

    #[test]
    fn webauthn_no_up_uv_flag_rejected() {
        let mut data = vec![0u8; MIN_AUTH_DATA_LEN + 64];
        data[32] = 0; // neither UP nor UV
        let sig = PrimitiveSignature::WebAuthn(WebAuthnSignature {
            r: B256::ZERO,
            s: B256::ZERO,
            pub_key_x: B256::ZERO,
            pub_key_y: B256::ZERO,
            webauthn_data: Bytes::from(data),
        });
        assert!(sig.recover_signer(&B256::ZERO).is_err());
    }

    #[test]
    fn webauthn_at_flag_rejected() {
        let mut data = vec![0u8; MIN_AUTH_DATA_LEN + 64];
        data[32] = WA_UP | WA_AT; // attestation flag not allowed for assertion
        let sig = PrimitiveSignature::WebAuthn(WebAuthnSignature {
            r: B256::ZERO,
            s: B256::ZERO,
            pub_key_x: B256::ZERO,
            pub_key_y: B256::ZERO,
            webauthn_data: Bytes::from(data),
        });
        assert!(sig.recover_signer(&B256::ZERO).is_err());
    }

    #[test]
    fn webauthn_ed_flag_rejected() {
        let mut data = vec![0u8; MIN_AUTH_DATA_LEN + 64];
        data[32] = WA_UP | WA_ED; // extensions not supported
        let sig = PrimitiveSignature::WebAuthn(WebAuthnSignature {
            r: B256::ZERO,
            s: B256::ZERO,
            pub_key_x: B256::ZERO,
            pub_key_y: B256::ZERO,
            webauthn_data: Bytes::from(data),
        });
        assert!(sig.recover_signer(&B256::ZERO).is_err());
    }

    #[test]
    fn webauthn_round_trip_end_to_end() {
        use p256::ecdsa::{signature::hazmat::PrehashSigner, Signature as P256Signature, SigningKey};
        use p256::elliptic_curve::rand_core::OsRng;

        // 1. clientDataJSON with challenge == base64url(tx_hash).
        let tx_hash = B256::from([0xCC; 32]);
        let challenge_b64 = URL_SAFE_NO_PAD.encode(tx_hash.as_slice());
        let client_data_json = format!(
            r#"{{"type":"webauthn.get","challenge":"{}","origin":"https://example.com"}}"#,
            challenge_b64
        );

        // 2. authenticatorData (37 bytes, UP only).
        let mut auth_data = vec![0u8; MIN_AUTH_DATA_LEN];
        auth_data[32] = WA_UP;

        // 3. webauthn_data = authData || clientDataJSON.
        let mut webauthn_data = auth_data.clone();
        webauthn_data.extend_from_slice(client_data_json.as_bytes());

        // 4. messageHash = sha256(authData || sha256(clientDataJSON)).
        let client_hash = Sha256::digest(client_data_json.as_bytes());
        let mut hasher = Sha256::new();
        hasher.update(&auth_data);
        hasher.update(client_hash);
        let message_hash = B256::from_slice(&hasher.finalize());

        // 5. Sign with a P256 key.
        let signing_key = SigningKey::random(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let encoded = verifying_key.to_encoded_point(false);
        let pub_key_x = B256::from_slice(encoded.x().unwrap());
        let pub_key_y = B256::from_slice(encoded.y().unwrap());

        let sig: P256Signature = signing_key.sign_prehash(message_hash.as_slice()).unwrap();
        let normalized = sig.normalize_s().unwrap_or(sig);
        let r = B256::from_slice(&normalized.r().to_bytes());
        let s = B256::from_slice(&normalized.s().to_bytes());

        let prim = PrimitiveSignature::WebAuthn(WebAuthnSignature {
            r,
            s,
            pub_key_x,
            pub_key_y,
            webauthn_data: Bytes::from(webauthn_data),
        });

        let expected = derive_p256_address(&pub_key_x, &pub_key_y);
        let recovered = prim.recover_signer(&tx_hash).expect("WebAuthn recover");
        assert_eq!(recovered, expected);
    }

    #[test]
    fn webauthn_challenge_mismatch_rejected() {
        // Build a WebAuthn payload whose challenge does NOT match tx_hash.
        let tx_hash = B256::from([0xCC; 32]);
        let wrong_challenge = URL_SAFE_NO_PAD.encode(B256::from([0xDD; 32]).as_slice());
        let client_data_json = format!(
            r#"{{"type":"webauthn.get","challenge":"{}"}}"#,
            wrong_challenge
        );

        let mut auth_data = vec![0u8; MIN_AUTH_DATA_LEN];
        auth_data[32] = WA_UP;
        let mut webauthn_data = auth_data;
        webauthn_data.extend_from_slice(client_data_json.as_bytes());

        let sig = PrimitiveSignature::WebAuthn(WebAuthnSignature {
            r: B256::ZERO,
            s: B256::ZERO,
            pub_key_x: B256::ZERO,
            pub_key_y: B256::ZERO,
            webauthn_data: Bytes::from(webauthn_data),
        });
        // Should bail at challenge check, before signature verification.
        assert!(sig.recover_signer(&tx_hash).is_err());
    }

    #[test]
    fn webauthn_wrong_type_field_rejected() {
        let tx_hash = B256::from([0xCC; 32]);
        let challenge_b64 = URL_SAFE_NO_PAD.encode(tx_hash.as_slice());
        // type = "webauthn.create" instead of "webauthn.get"
        let client_data_json = format!(
            r#"{{"type":"webauthn.create","challenge":"{}"}}"#,
            challenge_b64
        );

        let mut auth_data = vec![0u8; MIN_AUTH_DATA_LEN];
        auth_data[32] = WA_UP;
        let mut webauthn_data = auth_data;
        webauthn_data.extend_from_slice(client_data_json.as_bytes());

        let sig = PrimitiveSignature::WebAuthn(WebAuthnSignature {
            r: B256::ZERO,
            s: B256::ZERO,
            pub_key_x: B256::ZERO,
            pub_key_y: B256::ZERO,
            webauthn_data: Bytes::from(webauthn_data),
        });
        assert!(sig.recover_signer(&tx_hash).is_err());
    }
}
