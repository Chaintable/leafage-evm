use alloy::primitives::U256;
use alloy::rpc::types::TransactionRequest;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};

/// Extended call request wrapping alloy's `TransactionRequest` with Tempo-specific fields.
///
/// Uses `Deref`/`DerefMut` to `TransactionRequest` so all existing field access
/// (e.g., `request.to`, `request.input`, `request.nonce`) works without changes.
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
