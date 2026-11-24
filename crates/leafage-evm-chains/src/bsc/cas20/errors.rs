//! CAS20 failure kinds and ABI custom errors.
//!
//! Port of go-bsc `core/vm/cas20_errors.go`. Business-rule failures are ABI custom errors;
//! malformed calldata and unknown selectors revert with empty returndata (BEP-702 3.2).

use alloy::primitives::{keccak256, B256, U256};

use super::abi::{abi_bytes, encode_tuple};

/// How a CAS20 call can fail, in go-bsc error terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cas20Err {
    /// `ErrExecutionReverted` (empty data) or a `cas20RevertError` (custom error data).
    Revert(Vec<u8>),
    /// `ErrOutOfGas`.
    OutOfGas,
    /// `ErrWriteProtection`: reported as `StaticCallNotAllowed()` by `finishCAS20`.
    WriteProtection,
    /// `ErrCAS20DelegateCall`: reported as `DelegateCallNotAllowed()` by `finishCAS20`.
    DelegateCall,
}

pub type Cas20Result<T> = Result<T, Cas20Err>;

/// `ErrExecutionReverted` with empty returndata.
pub(crate) const REVERTED: Cas20Err = Cas20Err::Revert(Vec::new());

/// The 4-byte selector of a custom error or function signature.
pub(crate) fn selector(sig: &str) -> [u8; 4] {
    keccak256(sig.as_bytes())[..4].try_into().unwrap()
}

/// `revCAS20`: a custom error with static word arguments.
pub(crate) fn rev(sig: &str, words: &[B256]) -> Cas20Err {
    let mut data = Vec::with_capacity(4 + 32 * words.len());
    data.extend_from_slice(&selector(sig));
    for w in words {
        data.extend_from_slice(w.as_slice());
    }
    Cas20Err::Revert(data)
}

/// `revCAS20Bytes`: a custom error with one `bytes` argument.
pub(crate) fn rev_bytes(sig: &str, payload: &[u8]) -> Cas20Err {
    let mut data = selector(sig).to_vec();
    data.extend(encode_tuple(vec![abi_bytes(payload)]));
    Cas20Err::Revert(data)
}

/// `revPanic`: only 0x11 arises (arithmetic overflow).
pub(crate) fn rev_panic(code: u8) -> Cas20Err {
    rev("Panic(uint256)", &[w_u8(code)])
}

pub(crate) fn w_u256(v: U256) -> B256 {
    B256::from(v)
}

pub(crate) fn w_u64(v: u64) -> B256 {
    B256::from(U256::from(v))
}

pub(crate) fn w_u8(v: u8) -> B256 {
    let mut h = B256::ZERO;
    h[31] = v;
    h
}
