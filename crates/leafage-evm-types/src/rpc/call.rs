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
pub struct CallRequest {
    #[serde(flatten)]
    pub inner: TransactionRequest,

    /// Tempo batch calls for AA tx (type 0x76).
    /// Each call in the batch is executed atomically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "aaAuthorizationList"
    )]
    pub tempo_authorization_list: Option<Vec<TempoAuthGasInfo>>,
}

/// Lightweight key authorization info for gas estimation.
///
/// Full `SignedKeyAuthorization` has complex nested types; this captures
/// only the fields that affect gas: the signature type and limits count.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TempoKeyAuthGasInfo {
    /// Signature type on the key authorization ("secp256k1", "p256", "webauthn").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig_type: Option<String>,

    /// Number of spending limits. Each limit costs KEY_AUTH_PER_LIMIT_GAS (22k pre-T1B)
    /// or SSTORE_SET (250k post-T1B) in gas.
    #[serde(default)]
    pub num_limits: u32,
}

/// Lightweight per-authorization info for gas estimation.
///
/// Captures signature type and nonce from `TempoSignedAuthorization`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TempoAuthGasInfo {
    /// Signature type ("secp256k1", "p256", "webauthn").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig_type: Option<String>,

    /// Nonce of the authorization. When 0, incurs TIP-1000 account creation cost.
    #[serde(default)]
    pub nonce: u64,
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
