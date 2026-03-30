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

    /// Fee payer (sponsor) address. When specified, the sponsor's fee token balance
    /// is used for the estimateGas gas cap instead of the caller's.
    /// Writer recovers this from fee_payer_signature; for RPC we accept it directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_payer: Option<Address>,

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
